//! Parse hotkey strings like "F10" or "Ctrl+Shift+S" into RegisterHotKey arguments.
//! (Platform independent so it can be unit tested anywhere.)

pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;
pub const MOD_NOREPEAT: u32 = 0x4000;

/// Returns (modifiers incl. MOD_NOREPEAT, virtual-key code).
pub fn parse(s: &str) -> Result<(u32, u32), String> {
    let mut mods = MOD_NOREPEAT;
    let mut vk = None;
    for part in s.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        let up = part.to_ascii_uppercase();
        match up.as_str() {
            "CTRL" | "CONTROL" => mods |= MOD_CONTROL,
            "ALT" => mods |= MOD_ALT,
            "SHIFT" => mods |= MOD_SHIFT,
            "WIN" | "SUPER" => mods |= MOD_WIN,
            key => {
                if vk.is_some() {
                    return Err(format!("hotkey '{s}' has more than one key"));
                }
                vk = Some(
                    key_code(key).ok_or_else(|| format!("unknown key '{part}' in hotkey '{s}'"))?,
                );
            }
        }
    }
    vk.map(|v| (mods, v))
        .ok_or_else(|| format!("hotkey '{s}' has no key"))
}

fn key_code(k: &str) -> Option<u32> {
    let b = k.as_bytes();
    if b.len() == 1 && (b[0].is_ascii_uppercase() || b[0].is_ascii_digit()) {
        return Some(b[0] as u32); // VK_A..VK_Z, VK_0..VK_9 equal ASCII
    }
    if let Some(n) = k.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0x70 + n - 1);
        }
    }
    if let Some(n) = k.strip_prefix("NUMPAD").and_then(|n| n.parse::<u32>().ok()) {
        if n <= 9 {
            return Some(0x60 + n);
        }
    }
    Some(match k {
        "INSERT" | "INS" => 0x2D,
        "DELETE" | "DEL" => 0x2E,
        "HOME" => 0x24,
        "END" => 0x23,
        "PAGEUP" | "PGUP" => 0x21,
        "PAGEDOWN" | "PGDN" => 0x22,
        "PAUSE" => 0x13,
        "SCROLLLOCK" => 0x91,
        "SPACE" => 0x20,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        assert_eq!(parse("F10"), Ok((MOD_NOREPEAT, 0x79)));
        assert_eq!(
            parse("ctrl + shift + s"),
            Ok((MOD_NOREPEAT | MOD_CONTROL | MOD_SHIFT, b'S' as u32))
        );
        assert_eq!(parse("Alt+Numpad5"), Ok((MOD_NOREPEAT | MOD_ALT, 0x65)));
        assert_eq!(parse("Win+F24"), Ok((MOD_NOREPEAT | MOD_WIN, 0x87)));
    }

    #[test]
    fn rejects() {
        assert!(parse("Ctrl").is_err());
        assert!(parse("F25").is_err());
        assert!(parse("A+B").is_err());
        assert!(parse("Hyper+X").is_err());
    }
}
