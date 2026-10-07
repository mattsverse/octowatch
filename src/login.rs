//! Native login registration. The OS registration is authoritative: merely
//! starting the app never re-enables an item the user disabled elsewhere.

use anyhow::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Off,
    On,
    NeedsApproval,
    NeedsRepair,
    Unavailable(String),
}

impl State {
    pub fn requested(&self) -> bool {
        matches!(self, Self::On | Self::NeedsApproval)
    }
}

pub fn status() -> Result<State> {
    if cfg!(debug_assertions) {
        return Ok(State::Unavailable(
            "Available in installed release builds.".into(),
        ));
    }
    platform::status()
}

pub fn set_enabled(enabled: bool) -> Result<State> {
    if let State::Unavailable(reason) = status()? {
        anyhow::bail!("{reason}");
    }
    platform::set_enabled(enabled)
}

/// Called from GPUI's did-finish-launching callback, while the initial Apple
/// event is still available. `--background` also supports a quiet manual launch.
pub fn is_background_launch() -> bool {
    let explicit = std::env::args_os().skip(1).any(|arg| arg == "--background");
    #[cfg(target_os = "macos")]
    let login = platform::launched_at_login();
    #[cfg(not(target_os = "macos"))]
    let login = false;
    explicit || login
}

pub fn should_show_window(background: bool, tray_available: bool) -> bool {
    !background || !tray_available
}

#[cfg(target_os = "macos")]
mod platform {
    use std::path::PathBuf;

    use anyhow::{Context as _, Result, bail};
    use objc2::{
        msg_send,
        rc::Retained,
        runtime::{AnyClass, AnyObject},
    };
    use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventManager, NSError, NSProcessInfo};

    use super::State;

    // Runtime class lookup keeps older macOS versions able to run normally.
    // ServiceManagement itself predates SMAppService and exists on those OSes.
    #[link(name = "ServiceManagement", kind = "framework")]
    unsafe extern "C" {}

    fn service() -> Result<Option<Retained<AnyObject>>> {
        if NSProcessInfo::processInfo()
            .operatingSystemVersion()
            .majorVersion
            < 13
        {
            return Ok(None);
        }
        let executable = std::env::current_exe()?.canonicalize()?;
        let bundle: Option<PathBuf> = executable
            .ancestors()
            .find(|path| path.extension().is_some_and(|ext| ext == "app"))
            .map(PathBuf::from);
        let Some(bundle) = bundle else {
            return Ok(None);
        };
        // A bundle on a mounted installer or in a build directory isn't a
        // durable login target. Installation follows the README's convention.
        let user_apps = dirs::home_dir().map(|home| home.join("Applications"));
        if !bundle.starts_with("/Applications")
            && !user_apps.is_some_and(|apps| bundle.starts_with(apps))
        {
            return Ok(None);
        }
        let class = AnyClass::get(c"SMAppService").context("SMAppService is unavailable")?;
        // SAFETY: SMAppService's documented class property returns an object
        // with these documented instance methods. All calls run on the UI thread.
        Ok(Some(unsafe { msg_send![class, mainAppService] }))
    }

    fn service_status(service: &AnyObject) -> Result<State> {
        // SAFETY: `service` is the SMAppService returned by mainAppService.
        let status: isize = unsafe { msg_send![service, status] };
        super::main_app_status(status)
    }

    pub fn status() -> Result<State> {
        match service()? {
            Some(service) => service_status(&service),
            None => Ok(State::Unavailable(
                "Requires macOS 13 or later and a signed app installed in Applications.".into(),
            )),
        }
    }

    pub fn set_enabled(enabled: bool) -> Result<State> {
        let service = service()?.context("launch at login is unavailable for this app")?;
        let current = service_status(&service)?;
        if current.requested() == enabled {
            return Ok(current);
        }
        let mut error: Option<Retained<NSError>> = None;
        // SAFETY: documented SMAppService methods take an autoreleasing
        // NSError**; objc2 handles retaining the error through writeback.
        let success: bool = unsafe {
            if enabled {
                msg_send![&service, registerAndReturnError: &mut error]
            } else {
                msg_send![&service, unregisterAndReturnError: &mut error]
            }
        };
        if !success {
            // Registration may need the user to approve it in System Settings.
            if enabled && service_status(&service)? == State::NeedsApproval {
                return Ok(State::NeedsApproval);
            }
            bail!(
                "{}",
                error
                    .map(|err| err.localizedDescription().to_string())
                    .unwrap_or_else(|| "macOS could not change the login item".into())
            );
        }
        let observed = service_status(&service)?;
        if observed.requested() != enabled {
            bail!("macOS did not confirm the requested login item setting");
        }
        Ok(observed)
    }

    pub fn launched_at_login() -> bool {
        let Some(event) = NSAppleEventManager::sharedAppleEventManager().currentAppleEvent() else {
            return false;
        };
        // Apple's Launch Apple Event Constants: `oapp` with a `prdt`
        // parameter whose enum code is `lgit` identifies a login launch.
        // SAFETY: these are documented NSAppleEventDescriptor methods. The
        // four-character constants use their CoreServices u32 ABI.
        unsafe {
            let event_id: u32 = msg_send![&event, eventID];
            let property: Option<Retained<NSAppleEventDescriptor>> =
                msg_send![&event, paramDescriptorForKeyword: u32::from_be_bytes(*b"prdt")];
            let property_code: u32 = property
                .map(|property| msg_send![&property, enumCodeValue])
                .unwrap_or_default();
            super::is_login_event(event_id, property_code)
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn is_login_event(event_id: u32, property_code: u32) -> bool {
    event_id == u32::from_be_bytes(*b"oapp") && property_code == u32::from_be_bytes(*b"lgit")
}

#[cfg(any(target_os = "macos", test))]
fn main_app_status(status: isize) -> Result<State> {
    match status {
        // A freshly installed main app can report NotFound before its first
        // registration. It is off; allow an explicit registration attempt so
        // SMAppService can report the actual signature/registration error.
        0 | 3 => Ok(State::Off),
        1 => Ok(State::On),
        2 => Ok(State::NeedsApproval),
        _ => anyhow::bail!("unknown macOS login item status: {status}"),
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use anyhow::{Context as _, Result};

    use super::{State, linux};

    fn executable() -> Result<Option<std::path::PathBuf>> {
        let current = std::env::current_exe()?.canonicalize()?;
        linux::installed_executable(&current, std::env::var_os("APPIMAGE").as_deref())
    }

    fn entry() -> Result<std::path::PathBuf> {
        Ok(dirs::config_dir()
            .context("no config directory")?
            .join("autostart/com.matteogassend.octowatcher.desktop"))
    }

    pub fn status() -> Result<State> {
        let Some(executable) = executable()? else {
            return Ok(State::Unavailable(
                "Available for installed packages and AppImages. Move the app to a permanent location first.".into(),
            ));
        };
        linux::status_at(&entry()?, &executable)
    }

    pub fn set_enabled(enabled: bool) -> Result<State> {
        let executable =
            executable()?.context("launch at login requires an installed package or AppImage")?;
        linux::set_at(&entry()?, &executable, enabled)?;
        status()
    }
}

// Build and test the Linux file handling on either supported development OS.
#[cfg(any(target_os = "linux", test))]
mod linux {
    use std::{
        ffi::OsStr,
        fs, io,
        path::{Path, PathBuf},
    };

    use anyhow::{Context as _, Result, bail};

    use super::State;

    pub fn installed_executable(
        current: &Path,
        appimage: Option<&OsStr>,
    ) -> Result<Option<PathBuf>> {
        if let Some(appimage) = appimage {
            let appimage = PathBuf::from(appimage);
            if !appimage.is_absolute() || !appimage.is_file() {
                bail!("APPIMAGE must name an existing absolute path");
            }
            // Never register the read-only /tmp/.mount_... executable.
            return Ok(Some(appimage.canonicalize()?));
        }
        Ok(matches!(
            current.to_str(),
            Some("/usr/bin/octowatcher" | "/usr/local/bin/octowatcher")
        )
        .then(|| current.to_path_buf()))
    }

    pub fn status_at(entry: &Path, executable: &Path) -> Result<State> {
        let contents = match fs::read_to_string(entry) {
            Ok(contents) => contents,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(State::Off),
            Err(err) => return Err(err.into()),
        };
        // Honor an external desktop autostart toggle. Only inspect the main
        // group, so an unrelated action doesn't accidentally disable startup.
        let mut main_group = false;
        let mut application = false;
        let mut command = None;
        for line in contents.lines().map(str::trim) {
            if line.starts_with('[') {
                main_group = line == "[Desktop Entry]";
            } else if main_group && let Some((key, value)) = line.split_once('=') {
                match (key.trim(), value.trim()) {
                    ("Hidden", "true") | ("X-GNOME-Autostart-enabled", "false") => {
                        return Ok(State::Off);
                    }
                    ("Type", "Application") => application = true,
                    ("Exec", value) if !value.is_empty() => command = Some(value),
                    _ => {}
                }
            }
        }
        Ok(match (application, command) {
            (true, Some(command))
                if command == format!("{} --background", desktop_exec(executable)?) =>
            {
                State::On
            }
            (true, Some(_)) => State::NeedsRepair,
            _ => State::Off,
        })
    }

    pub fn set_at(entry: &Path, executable: &Path, enabled: bool) -> Result<()> {
        if !enabled {
            return match fs::remove_file(entry) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(err.into()),
            };
        }
        let exec = desktop_exec(executable)?;
        let contents = format!(
            "[Desktop Entry]\nType=Application\nName=Octowatcher\nComment=Check for GitHub review requests in the background\nExec={exec} --background\nIcon=octowatcher\nTerminal=false\n"
        );
        fs::create_dir_all(entry.parent().context("autostart entry has no parent")?)?;
        let temporary = entry.with_extension("desktop.tmp");
        fs::write(&temporary, contents)?;
        fs::rename(&temporary, entry)?;
        Ok(())
    }

    fn desktop_exec(executable: &Path) -> Result<String> {
        let path = executable
            .to_str()
            .context("the startup path must be UTF-8")?;
        if !executable.is_absolute() || path.contains(['\n', '\r', '\0']) {
            bail!("the startup path must be absolute and contain no line breaks");
        }
        // Desktop Entry Exec quoting followed by string-value escaping: each
        // backslash in the quoted argument needs escaping again in the file.
        // Percent escapes prevent paths being interpreted as field codes.
        let mut argument = String::new();
        for character in path.chars() {
            match character {
                '%' => argument.push_str("%%"),
                '\\' | '"' | '`' | '$' => {
                    argument.push('\\');
                    argument.push(character);
                }
                _ => argument.push(character),
            }
        }
        Ok(format!("\"{}\"", argument.replace('\\', "\\\\")))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn registers_quiet_login_and_respects_external_disable() {
            let directory = tempfile::tempdir().unwrap();
            let entry = directory.path().join("autostart/octowatcher.desktop");
            let executable = Path::new("/home/user/My Apps/Octowatcher.AppImage");
            assert_eq!(status_at(&entry, executable).unwrap(), State::Off);
            set_at(&entry, executable, true).unwrap();
            assert_eq!(status_at(&entry, executable).unwrap(), State::On);
            let contents = fs::read_to_string(&entry).unwrap();
            assert!(
                contents
                    .contains("Exec=\"/home/user/My Apps/Octowatcher.AppImage\" --background\n")
            );
            fs::write(&entry, format!("{contents}Hidden=true\n")).unwrap();
            assert_eq!(status_at(&entry, executable).unwrap(), State::Off);
            set_at(&entry, executable, true).unwrap();
            assert_eq!(status_at(&entry, executable).unwrap(), State::On);
            set_at(&entry, executable, false).unwrap();
            set_at(&entry, executable, false).unwrap();
            assert_eq!(status_at(&entry, executable).unwrap(), State::Off);
        }

        #[test]
        fn moved_appimage_needs_explicit_repair_without_overriding_disable() {
            let directory = tempfile::tempdir().unwrap();
            let entry = directory.path().join("autostart/octowatcher.desktop");
            let old = directory.path().join("Old %f.AppImage");
            let new = directory.path().join("New $ name.AppImage");
            fs::write(&old, "image").unwrap();
            set_at(&entry, &old, true).unwrap();
            assert_eq!(status_at(&entry, &old).unwrap(), State::On);
            fs::rename(&old, &new).unwrap();
            assert_eq!(status_at(&entry, &new).unwrap(), State::NeedsRepair);
            assert!(!State::NeedsRepair.requested());
            let stale = fs::read_to_string(&entry).unwrap();
            // A status read never silently re-registers the running copy.
            assert_eq!(fs::read_to_string(&entry).unwrap(), stale);
            fs::write(&entry, format!("{stale}Hidden=true\n")).unwrap();
            assert_eq!(status_at(&entry, &new).unwrap(), State::Off);
            fs::write(&entry, stale).unwrap();
            set_at(&entry, &new, true).unwrap();
            assert_eq!(status_at(&entry, &new).unwrap(), State::On);
            set_at(&entry, &new, false).unwrap();
            assert_eq!(status_at(&entry, &new).unwrap(), State::Off);
        }

        #[test]
        fn quotes_paths_and_escapes_exec_field_codes() {
            assert_eq!(
                desktop_exec(Path::new("/apps/a b%f\"$`\\.AppImage")).unwrap(),
                "\"/apps/a b%%f\\\\\"\\\\$\\\\`\\\\\\\\.AppImage\""
            );
            assert!(desktop_exec(Path::new("/apps/new\nline")).is_err());
            assert!(desktop_exec(Path::new("relative.AppImage")).is_err());
        }

        #[test]
        fn failed_registration_preserves_entry_and_incomplete_entries_are_off() {
            let directory = tempfile::tempdir().unwrap();
            let entry = directory.path().join("autostart/octowatcher.desktop");
            let executable = Path::new("/usr/bin/octowatcher");
            set_at(&entry, executable, true).unwrap();
            let previous = fs::read_to_string(&entry).unwrap();
            assert!(set_at(&entry, Path::new("/apps/bad\npath"), true).is_err());
            assert_eq!(fs::read_to_string(&entry).unwrap(), previous);
            fs::write(&entry, "[Desktop Entry]\nType=Application\n").unwrap();
            assert_eq!(status_at(&entry, executable).unwrap(), State::Off);
            fs::write(
                &entry,
                format!("{previous}X-GNOME-Autostart-enabled = false\n"),
            )
            .unwrap();
            assert_eq!(status_at(&entry, executable).unwrap(), State::Off);
        }

        #[test]
        fn uses_appimage_path_instead_of_ephemeral_mount() {
            let directory = tempfile::tempdir().unwrap();
            let appimage = directory.path().join("Octowatcher.AppImage");
            fs::write(&appimage, "image").unwrap();
            let mount = Path::new("/tmp/.mount_octowatcher/usr/bin/octowatcher");
            assert_eq!(
                installed_executable(mount, Some(appimage.as_os_str())).unwrap(),
                Some(appimage.canonicalize().unwrap())
            );
            assert_eq!(installed_executable(mount, None).unwrap(), None);
            assert_eq!(
                installed_executable(Path::new("/usr/bin/octowatcher"), None).unwrap(),
                Some(PathBuf::from("/usr/bin/octowatcher"))
            );
            assert_eq!(
                installed_executable(
                    Path::new("/home/user/project/target/release/octowatcher"),
                    None
                )
                .unwrap(),
                None
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_launches_show_and_quiet_launches_need_a_tray() {
        assert!(should_show_window(false, true));
        assert!(should_show_window(false, false));
        assert!(!should_show_window(true, true));
        assert!(should_show_window(true, false));
    }

    #[test]
    fn detects_login_event_without_treating_reopen_as_login() {
        assert!(is_login_event(
            u32::from_be_bytes(*b"oapp"),
            u32::from_be_bytes(*b"lgit")
        ));
        assert!(!is_login_event(
            u32::from_be_bytes(*b"rapp"),
            u32::from_be_bytes(*b"lgit")
        ));
        assert!(!is_login_event(u32::from_be_bytes(*b"oapp"), 0));
    }

    #[test]
    fn new_main_apps_can_register_and_approval_is_distinct_from_enabled() {
        assert_eq!(main_app_status(0).unwrap(), State::Off);
        assert_eq!(main_app_status(3).unwrap(), State::Off);
        assert_eq!(main_app_status(1).unwrap(), State::On);
        assert_eq!(main_app_status(2).unwrap(), State::NeedsApproval);
        assert!(main_app_status(2).unwrap().requested());
        assert!(main_app_status(99).is_err());
    }
}
