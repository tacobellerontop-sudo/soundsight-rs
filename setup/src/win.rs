use std::path::{Path, PathBuf};

use windows::core::{w, BOOL, GUID, HSTRING, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::payload::FILES;

/// Must match the hardware ID that `driver/prepare.py` writes into the INF.
const HARDWARE_ID: &str = r"ROOT\SoundSight71";
const TITLE: PCWSTR = w!("SoundSight 7.1 Setup");

pub fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).map(|a| a.to_ascii_lowercase()).collect();
    let quiet = args.iter().any(|a| a == "--quiet" || a == "/quiet");
    let action = args.iter().find(|a| !a.starts_with('-') && !a.starts_with('/')).cloned();

    let action = match action.as_deref() {
        Some("install") => Action::Install,
        Some("uninstall") => Action::Uninstall,
        Some(other) => return fail(quiet, &format!("Unknown command \"{other}\". Use install or uninstall.")),
        None if quiet => return fail(quiet, "Say install or uninstall."),
        None => match ask(
            "Install the SoundSight 7.1 virtual speakers?\n\n\
             Yes: install (or repair)\nNo: uninstall\nCancel: do nothing",
            MB_YESNOCANCEL | MB_ICONQUESTION,
        ) {
            IDYES => Action::Install,
            IDNO => Action::Uninstall,
            _ => return 0,
        },
    };

    let result = match action {
        Action::Install => install(),
        Action::Uninstall => uninstall(),
    };
    match result {
        Ok(msg) => {
            if !quiet {
                ask(&msg, MB_OK | MB_ICONINFORMATION);
            }
            0
        }
        Err(msg) => fail(quiet, &msg),
    }
}

enum Action {
    Install,
    Uninstall,
}

fn ask(text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    unsafe { MessageBoxW(None, &HSTRING::from(text), TITLE, style) }
}

fn fail(quiet: bool, msg: &str) -> i32 {
    if quiet {
        eprintln!("{msg}");
    } else {
        ask(msg, MB_OK | MB_ICONERROR);
    }
    1
}

/// Where the driver package is unpacked. It stays there so uninstall can find the INF.
fn package_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("SoundSight").join("driver")
}

fn inf_path(dir: &Path) -> Option<PathBuf> {
    FILES.iter().find(|(n, _)| n.to_ascii_lowercase().ends_with(".inf")).map(|(n, _)| dir.join(n))
}

/// Hardware ID as a REG_MULTI_SZ: UTF-16, NUL-terminated, plus a final NUL.
fn hwid_multi_sz() -> Vec<u8> {
    HARDWARE_ID.encode_utf16().chain([0, 0]).flat_map(u16::to_le_bytes).collect()
}

fn install() -> Result<String, String> {
    if FILES.is_empty() {
        return Err("This build of the setup has no driver inside it. Use the SoundSight71Setup.exe built by CI.".into());
    }
    let dir = package_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
    for (name, bytes) in FILES {
        std::fs::write(dir.join(name), bytes).map_err(|e| format!("Could not write {name}: {e}"))?;
    }
    let inf = inf_path(&dir).expect("payload has an INF");
    let inf_w = HSTRING::from(inf.as_os_str());

    if find_devices().is_empty() {
        create_root_device(&inf_w)?;
    }

    let mut reboot = BOOL(0);
    unsafe {
        UpdateDriverForPlugAndPlayDevicesW(None, &HSTRING::from(HARDWARE_ID), &inf_w, INSTALLFLAG_FORCE, Some(&mut reboot))
    }
    .map_err(|e| format!("Windows refused to install the driver: {e}"))?;

    for devinst in find_devices() {
        let (mut status, mut problem) = (CM_DEVNODE_STATUS_FLAGS(0), CM_PROB(0));
        let cr = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) };
        if cr == CR_SUCCESS && (status.0 & DN_HAS_PROBLEM.0) != 0 {
            if problem == CM_PROB_UNSIGNED_DRIVER {
                return Err("The driver is installed but Windows will not load it because it is not signed by Microsoft \
                            (Device Manager code 52).\n\nThis is a developer build. It only runs with test signing turned on, \
                            which games with anti-cheat do not allow. Run \"SoundSight71Setup uninstall\" to remove it."
                    .into());
            }
            return Err(format!("The driver is installed but the device reports problem code {}. See Device Manager.", problem.0));
        }
    }

    let mut msg = String::from(
        "Installed. \"Speakers (SoundSight 7.1 Virtual Speakers)\" is now in Windows Sound settings.\n\n\
         To use it, set it as the default output device and set its speaker setup to 7.1 \
         (Sound settings > SoundSight 7.1 > Configure).",
    );
    if reboot.as_bool() {
        msg += "\n\nRestart Windows to finish.";
    }
    Ok(msg)
}

fn create_root_device(inf: &HSTRING) -> Result<(), String> {
    let mut class = GUID::zeroed();
    let mut class_name = [0u16; MAX_CLASS_NAME_LEN as usize];
    unsafe { SetupDiGetINFClassW(inf, &mut class, &mut class_name, None) }.map_err(|e| format!("Bad driver INF: {e}"))?;

    let set = unsafe { SetupDiCreateDeviceInfoList(Some(&class), None) }.map_err(|e| e.to_string())?;
    let result = (|| {
        let mut data = SP_DEVINFO_DATA { cbSize: size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
        unsafe {
            SetupDiCreateDeviceInfoW(set, PCWSTR(class_name.as_ptr()), &class, None, None, DICD_GENERATE_ID, Some(&mut data))?;
            SetupDiSetDeviceRegistryPropertyW(set, &mut data, SPDRP_HARDWAREID, Some(&hwid_multi_sz()))?;
            SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set, Some(&data))
        }
    })()
    .map_err(|e| format!("Could not create the virtual device: {e}"));
    unsafe {
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
    result
}

/// Device instances (present or not) whose hardware ID is ours.
fn find_devices() -> Vec<u32> {
    let mut out = Vec::new();
    for_each_device(|_, data| {
        out.push(data.DevInst);
    });
    out
}

fn for_each_device(mut f: impl FnMut(HDEVINFO, &SP_DEVINFO_DATA)) {
    let Ok(set) = (unsafe { SetupDiGetClassDevsW(None, w!("ROOT"), None, DIGCF_ALLCLASSES) }) else {
        return;
    };
    let mut i = 0;
    loop {
        let mut data = SP_DEVINFO_DATA { cbSize: size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
        if unsafe { SetupDiEnumDeviceInfo(set, i, &mut data) }.is_err() {
            break;
        }
        i += 1;
        let mut buf = [0u8; 1024];
        if unsafe { SetupDiGetDeviceRegistryPropertyW(set, &data, SPDRP_HARDWAREID, None, Some(&mut buf), None) }.is_err() {
            continue;
        }
        let ids: Vec<u16> = buf.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let matches = ids
            .split(|&c| c == 0)
            .take_while(|s| !s.is_empty())
            .any(|s| String::from_utf16_lossy(s).eq_ignore_ascii_case(HARDWARE_ID));
        if matches {
            f(set, &data);
        }
    }
    unsafe {
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
}

fn uninstall() -> Result<String, String> {
    let mut removed = 0;
    let mut errors = Vec::new();
    for_each_device(|set, data| match unsafe { DiUninstallDevice(HWND::default(), set, data, 0, None) } {
        Ok(()) => removed += 1,
        Err(e) => errors.push(e.to_string()),
    });

    let dir = package_dir();
    if let Some(inf) = inf_path(&dir).filter(|p| p.exists()) {
        if let Err(e) = unsafe { DiUninstallDriverW(None, &HSTRING::from(inf.as_os_str()), DIUNINSTALLDRIVER_FLAGS(0), None) } {
            errors.push(format!("removing the driver package: {e}"));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);

    if !errors.is_empty() {
        return Err(format!("Uninstall finished with errors:\n{}", errors.join("\n")));
    }
    Ok(if removed > 0 {
        "SoundSight 7.1 virtual speakers removed.".into()
    } else {
        "SoundSight 7.1 virtual speakers were not installed.".into()
    })
}
