//! IInputManager's immutable virtual keyboard. The InputDevice body is written
//! by the live framework, including its real native KeyCharacterMap parcel.
//! `reply` accepts internal semantic operation IDs, not Binder transaction
//! numbers. The broker maps the API-37 Stub's installed codes to these IDs.

pub const GAMEPAD_DEVICE_ID: i32 = 2;

fn int(args: &[u8], index: usize) -> Option<i32> {
    Some(i32::from_le_bytes(args.get(index * 4..index * 4 + 4)?.try_into().ok()?))
}

fn supported(key: i32) -> bool {
    matches!(key, 7..=16 | 19..=22 | 29..=62 | 66..=76 | 92 | 93 | 111..=115 | 117 | 118 | 122..=124)
}

fn supported_gamepad_key(key: i32) -> bool {
    matches!(key, 19..=23 | 96..=110)
}

/// Reply body following AIDL's exception status. Unknown transactions are left
/// to the broker. Unknown device ids never alias the virtual keyboard.
pub fn reply(code: u32, args: &[u8], device: &[u8]) -> Option<Vec<u8>> {
    reply_with_devices(code, args, device, &[])
}

/// Like [`reply`], exposing both the virtual keyboard (`-1`) and the virtual
/// Xbox 360 gamepad (`2`) when registered.
pub fn reply_with_devices(
    code: u32,
    args: &[u8],
    keyboard: &[u8],
    gamepad: &[u8],
) -> Option<Vec<u8>> {
    let mut words = Vec::<i32>::new();
    match code {
        1 => {
            let id = int(args, 0);
            let chosen = match id {
                Some(-1) if !keyboard.is_empty() => Some(keyboard),
                Some(GAMEPAD_DEVICE_ID) if !gamepad.is_empty() => Some(gamepad),
                _ => None,
            };
            if let Some(device) = chosen {
                let mut out = 1i32.to_le_bytes().to_vec();
                out.extend_from_slice(device);
                return Some(out);
            }
            words.push(0); // null InputDevice
        }
        2 => {
            let mut ids = Vec::with_capacity(2);
            if !gamepad.is_empty() {
                ids.push(GAMEPAD_DEVICE_ID);
            }
            if !keyboard.is_empty() {
                ids.push(-1);
            }
            words.push(ids.len() as i32);
            words.extend(ids);
        }
        3 => {
            let id = int(args, 0);
            let enabled = (id == Some(-1) && !keyboard.is_empty())
                || (id == Some(GAMEPAD_DEVICE_ID) && !gamepad.is_empty());
            words.push(enabled as i32);
        }
        4 | 5 | 22 => {} // immutable devices; no hotplug notifications
        6 => {
            let count = int(args, 2)?;
            if !(0..=768).contains(&count) { return None; }
            let count = count as usize;
            let output = int(args, 3 + count)?;
            if !(0..=768).contains(&output) { return None; }
            let id = int(args, 0)?;
            let source = int(args, 1)?;
            let is_kb = id == -1 && !keyboard.is_empty() && (source == 0 || source & 0x100 != 0);
            let is_pad = id == GAMEPAD_DEVICE_ID
                && !gamepad.is_empty()
                && (source == 0 || source & 0x401 != 0 || source & 0x200 != 0);
            let valid = (is_kb || is_pad) && output as usize == count;
            words.extend([valid as i32, output]);
            for i in 0..output as usize {
                let has = valid
                    && int(args, 3 + i).is_some_and(|key| {
                        (is_kb && supported(key)) || (is_pad && supported_gamepad_key(key))
                    });
                words.push(has as i32);
            }
        }
        7 => {
            let id = int(args, 0)?;
            let key = int(args, 1)?;
            let has = (id == -1 && !keyboard.is_empty() && supported(key))
                || (id == GAMEPAD_DEVICE_ID && !gamepad.is_empty() && supported_gamepad_key(key));
            words.push(if has { key } else { 0 });
        }
        8 => {
            // `getVelocityTrackerStrategy()`. `VelocityTracker.toStrategyId`
            // maps any unrecognized name to DEFAULT, so "default" is always
            // accepted; the reply is a parcelled String16, not an int.
            return Some(crate::binder::string16("default"));
        }
        _ => return None,
    }
    Some(words.into_iter().flat_map(i32::to_le_bytes).collect())
}

/// Skip this build's Java writeInterfaceToken header and UTF-16 descriptor.
pub fn arguments(data: &[u8]) -> Option<&[u8]> {
    let token: Vec<u16> = "android.hardware.input.IInputManager".encode_utf16().collect();
    for start in [12usize, 0] {
        let n = u32::from_le_bytes(data.get(start..start + 4)?.try_into().ok()?) as usize;
        if n != token.len() { continue; }
        let text = data.get(start + 4..start + 4 + n * 2)?;
        if !text.chunks_exact(2).zip(&token).all(|(b, c)| u16::from_le_bytes([b[0], b[1]]) == *c) { continue; }
        if data.get(start + 4 + n * 2..start + 6 + n * 2)? != [0, 0] { continue; }
        let end = (start + 6 + n * 2 + 3) & !3;
        return data.get(end..);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bytes(words: &[i32]) -> Vec<u8> { words.iter().flat_map(|v| v.to_le_bytes()).collect() }

    #[test]
    fn enumeration_and_device_lookup_do_not_invent_other_devices() {
        assert_eq!(reply(2, &[], &[10]), Some(bytes(&[1, -1])));
        assert_eq!(reply(2, &[], &[]), Some(bytes(&[0])));
        assert_eq!(reply(1, &bytes(&[-1]), &[10]), Some([bytes(&[1]), vec![10]].concat()));
        assert_eq!(reply(1, &bytes(&[0]), &[10]), Some(bytes(&[0])));
    }

    #[test]
    fn key_availability_matches_the_physical_keyboard_subset() {
        assert_eq!(reply(6, &bytes(&[-1, 0x101, 3, 29, 67, 200, 3]), &[10]),
            Some(bytes(&[1, 3, 1, 1, 0])));
        assert_eq!(reply(6, &bytes(&[4, 0x101, 1, 29, 1]), &[10]), Some(bytes(&[0, 1, 0])));
        assert!(reply(6, &bytes(&[-1, 0, i32::MAX]), &[10]).is_none());
        assert!(reply(6, &bytes(&[-1, 0, 1, 29]), &[10]).is_none());
    }

    #[test]
    fn velocity_tracker_strategy_is_a_parcelled_default_string() {
        let reply = reply(8, &[], &[10]).unwrap();
        let len = u32::from_le_bytes(reply[..4].try_into().unwrap()) as usize;
        assert_eq!(len, 7); // "default"
        let text: Vec<u16> = reply[4..4 + len * 2]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(String::from_utf16(&text).unwrap(), "default");
        assert_eq!(&reply[4 + len * 2..4 + len * 2 + 2], &[0, 0]);
        assert_eq!(reply.len() % 4, 0);
    }

    #[test]
    fn malformed_interface_tokens_are_rejected() {
        assert!(arguments(&[0; 32]).is_none());
        let mut data = vec![0; 12];
        let token: Vec<u16> = "android.hardware.input.IInputManager".encode_utf16().collect();
        data.extend_from_slice(&(token.len() as u32).to_le_bytes());
        for c in token { data.extend_from_slice(&c.to_le_bytes()); }
        data.extend_from_slice(&[0, 0]);
        while data.len() % 4 != 0 { data.push(0); }
        data.extend_from_slice(&(-1i32).to_le_bytes());
        assert_eq!(arguments(&data), Some((-1i32).to_le_bytes().as_slice()));
        assert!(arguments(&data[..data.len() - 5]).is_none());
    }

    #[test]
    fn exposes_virtual_xbox_gamepad_alongside_keyboard() {
        assert_eq!(
            reply_with_devices(2, &[], &[10], &[20]),
            Some(bytes(&[2, GAMEPAD_DEVICE_ID, -1]))
        );
        assert_eq!(
            reply_with_devices(1, &bytes(&[GAMEPAD_DEVICE_ID]), &[10], &[20]),
            Some([bytes(&[1]), vec![20]].concat())
        );
        assert_eq!(
            reply_with_devices(3, &bytes(&[GAMEPAD_DEVICE_ID]), &[10], &[20]),
            Some(bytes(&[1]))
        );
        assert_eq!(
            reply_with_devices(6, &bytes(&[GAMEPAD_DEVICE_ID, 0x401, 3, 96, 108, 29, 3]), &[10], &[20]),
            Some(bytes(&[1, 3, 1, 1, 0]))
        );
    }
}
