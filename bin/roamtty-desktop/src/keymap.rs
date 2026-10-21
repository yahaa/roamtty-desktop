//! USB HID keyboard usages (page 0x07) to Linux evdev key codes; an X keycode is evdev + 8.
//! Keys travel by physical position, so the remote keyboard layout does not matter.

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
}
