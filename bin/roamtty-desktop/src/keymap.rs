//! USB HID keyboard usages (page 0x07) to Linux evdev key codes; an X keycode is evdev + 8.
//! Keys travel by physical position, so the remote keyboard layout does not matter.

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn hid_to_evdev(hid: u16) -> Option<u16> {
    let code = match hid {
        0x04..=0x1D => [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44][(hid - 0x04) as usize],
        0x1E..=0x27 => [2, 3, 4, 5, 6, 7, 8, 9, 10, 11][(hid - 0x1E) as usize],
        0x28..=0x39 => [28, 1, 14, 15, 57, 12, 13, 26, 27, 43, 43, 39, 40, 41, 51, 52, 53, 58][(hid - 0x28) as usize],
        0x3A..=0x45 => [59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 88][(hid - 0x3A) as usize],
        0x46..=0x52 => [99, 70, 119, 110, 102, 104, 111, 107, 109, 106, 105, 108, 103][(hid - 0x46) as usize],
        0x53..=0x63 => [69, 98, 55, 74, 78, 96, 79, 80, 81, 75, 76, 77, 71, 72, 73, 82, 83][(hid - 0x53) as usize],
        0x64 => 86,
        0x65 => 127,
        0x66 => 116,
        0x67 => 117,
        0x68..=0x73 => 183 + (hid - 0x68),
        0x7F => 113, // mute
        0x80 => 115, // volume up
        0x81 => 114, // volume down
        0xE0..=0xE7 => [29, 42, 56, 125, 97, 54, 100, 126][(hid - 0xE0) as usize],
        _ => return None,
    };
    Some(code)
}

/// The keysym a non-character key carries on every layout. X servers do not all number their keys
/// like evdev (xrdp's Xorg uses the old xfree86 codes, where evdev's Up is Print), so these keys are
/// found by what they mean; character keys keep going by position.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn hid_keysym(hid: u16) -> Option<u32> {
    let sym = match hid {
        0x28 => 0xFF0D, // Return
        0x29 => 0xFF1B, // Escape
        0x2A => 0xFF08, // BackSpace
        0x2B => 0xFF09, // Tab
        0x39 => 0xFFE5, // Caps_Lock
        0x3A..=0x45 => 0xFFBE + (hid - 0x3A) as u32, // F1..F12
        0x46 => 0xFF61, // Print
        0x47 => 0xFF14, // Scroll_Lock
        0x48 => 0xFF13, // Pause
        0x49 => 0xFF63, // Insert
        0x4A => 0xFF50, // Home
        0x4B => 0xFF55, // Prior
        0x4C => 0xFFFF, // Delete
        0x4D => 0xFF57, // End
        0x4E => 0xFF56, // Next
        0x4F => 0xFF53, // Right
        0x50 => 0xFF51, // Left
        0x51 => 0xFF54, // Down
        0x52 => 0xFF52, // Up
        0x53 => 0xFF7F, // Num_Lock
        0x54 => 0xFFAF, // KP_Divide
        0x55 => 0xFFAA, // KP_Multiply
        0x56 => 0xFFAD, // KP_Subtract
        0x57 => 0xFFAB, // KP_Add
        0x58 => 0xFF8D, // KP_Enter
        0x65 => 0xFF67, // Menu
        0x7F => 0x1008_FF12, // XF86AudioMute
        0x80 => 0x1008_FF13, // XF86AudioRaiseVolume
        0x81 => 0x1008_FF11, // XF86AudioLowerVolume
        0xE0 => 0xFFE3, // Control_L
        0xE1 => 0xFFE1, // Shift_L
        0xE2 => 0xFFE9, // Alt_L
        0xE3 => 0xFFEB, // Super_L
        0xE4 => 0xFFE4, // Control_R
        0xE5 => 0xFFE2, // Shift_R
        0xE6 => 0xFFEA, // Alt_R
        0xE7 => 0xFFEC, // Super_R
        _ => return None,
    };
    Some(sym)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const HID_LEFT_SHIFT: u16 = 0xE1;
pub const HID_LEFT_GUI: u16 = 0xE3;
pub const HID_D: u16 = 0x07;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_keys() {
        assert_eq!(hid_to_evdev(0x04), Some(30)); // A
        assert_eq!(hid_to_evdev(0x27), Some(11)); // 0
        assert_eq!(hid_to_evdev(0x28), Some(28));
        assert_eq!(hid_to_evdev(0x45), Some(88)); // F12
        assert_eq!(hid_to_evdev(0x52), Some(103)); // Up
        assert_eq!(hid_to_evdev(HID_LEFT_GUI), Some(125));
        assert_eq!(hid_to_evdev(0xE7), Some(126));
        assert_eq!(hid_to_evdev(0x00), None);
    }

    #[test]
    fn named_keys() {
        assert_eq!(hid_keysym(0x52), Some(0xFF52)); // Up
        assert_eq!(hid_keysym(0x45), Some(0xFFC9)); // F12
        assert_eq!(hid_keysym(HID_LEFT_GUI), Some(0xFFEB));
        assert_eq!(hid_keysym(0x04), None); // A goes by position
    }
}
