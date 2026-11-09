//! Optional guided 7.1 setup with Voicemeeter Banana (VB-Audio, free/donationware).
//!
//! SoundSight doesn't ship or download Voicemeeter itself: VB-Audio's licence doesn't
//! allow redistribution. This only detects an existing install, opens VB-Audio's
//! official download page, and walks through the Windows settings, one dialog at a time.

use super::util::wide;
use std::path::PathBuf;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

const DOWNLOAD_PAGE: &str = "https://vb-audio.com/Voicemeeter/banana.htm";
const UNINSTALL_KEYS: [&str; 2] = [
    r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\VB:Voicemeeter {17359A74-1236-5467}",
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\VB:Voicemeeter {17359A74-1236-5467}",
];
const BANANA_EXE: &str = "voicemeeterpro.exe";

/// Run the guided setup. `channels` = channel count SoundSight is capturing now (0 = none).
pub fn guided_setup(owner: HWND, channels: u32) {
    if channels >= 8 {
        msg(
            owner,
            "SoundSight is already getting 7.1 sound (8 channels) from your default output device.\n\n\
             Nothing to set up.",
            MB_OK | MB_ICONINFORMATION,
        );
        return;
    }

    let Some(exe) = find_banana() else {
        let r = msg(
            owner,
            "For full 7.1 surround, SoundSight can use Voicemeeter Banana, a free virtual \
             audio mixer by VB-Audio (donationware). It adds a 7.1 output device that games \
             play into and passes the sound on to your headset.\n\n\
             It is a separate install from VB-Audio. Open their official download page now?\n\n\
             After installing it, restart Windows, then click \"7.1 surround setup\" again.",
            MB_YESNO | MB_ICONQUESTION,
        );
        if r == IDYES {
            open(DOWNLOAD_PAGE, None);
        }
        return;
    };

    let steps = [
        "Step 1 of 3: Voicemeeter\n\n\
             Click OK to start Voicemeeter Banana. In its window, click \"A1\" (top right) \
             and pick your headset or speakers (choose the \"WDM\" entry).\n\n\
             This is where you'll hear the game from now on.",
        "Step 2 of 3: Windows sound settings\n\n\
             Click OK to open the Sound control panel. On the Playback tab:\n\
             1. Right-click \"Voicemeeter Input\" > Set as Default Device.\n\
             2. With it selected, click Configure > \"7.1 Surround\" > Next until Finish.\n\n\
             Games then output 7.1 into Voicemeeter.",
        "Step 3 of 3: check it\n\n\
             SoundSight follows the default output device, so it switches to Voicemeeter \
             by itself. Open SoundSight's Settings: the speaker test should now show 8 \
             channels while a game plays.\n\n\
             Tip: in Voicemeeter, Menu > \"Run on Windows Startup\" keeps it running.\n\
             To undo, set your headset as the default device again.",
    ];
    let exe_s = exe.to_string_lossy().into_owned();
    for (i, text) in steps.iter().enumerate() {
        if msg(owner, text, MB_OKCANCEL | MB_ICONINFORMATION) != IDOK {
            return;
        }
        match i {
            0 => open(&exe_s, None),
            1 => open("control.exe", Some("mmsys.cpl")),
            _ => {}
        }
    }
}

fn msg(owner: HWND, text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let t = wide(text);
    unsafe {
        MessageBoxW(
            Some(owner),
            PCWSTR(t.as_ptr()),
            w!("SoundSight: 7.1 surround setup"),
            style,
        )
    }
}

fn open(file: &str, params: Option<&str>) {
    let f = wide(file);
    let p = params.map(wide);
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(f.as_ptr()),
            p.as_ref().map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr())),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Path of voicemeeterpro.exe (Banana) if Voicemeeter is installed.
fn find_banana() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = UNINSTALL_KEYS
        .iter()
        .filter_map(|k| reg_string(k, "UninstallString"))
        .filter_map(|s| {
            PathBuf::from(s.trim_matches('"'))
                .parent()
                .map(PathBuf::from)
        })
        .collect();
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(pf) = std::env::var_os(var) {
            dirs.push(PathBuf::from(pf).join("VB").join("Voicemeeter"));
        }
    }
    dirs.into_iter()
        .map(|d| d.join(BANANA_EXE))
        .find(|p| p.is_file())
}

fn reg_string(key: &str, value: &str) -> Option<String> {
    use windows::Win32::System::Registry::*;
    let (k, v) = (wide(key), wide(value));
    let mut buf = [0u16; 520];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(k.as_ptr()),
            PCWSTR(v.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            Some(&mut size),
        )
    };
    if r.is_err() {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}
