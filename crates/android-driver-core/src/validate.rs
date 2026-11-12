use crate::error::{Result, invalid_argument};

pub const MAX_SERIAL_BYTES: usize = 128;
pub const MAX_USER: u32 = 999;
pub const MAX_DISPLAY_EDGE: u32 = 4096;
pub const MAX_DISPLAY_PIXELS: u32 = 4_000_000;
pub const MAX_SWIPE_MS: u32 = 10_000;

/// Keycodes that cannot change device-level state such as power, volume,
/// unlock, or the global media session. The allowlist is deliberately small.
pub const SAFE_KEYCODES: [u32; 13] = [
    4,   // KEYCODE_BACK
    19,  // KEYCODE_DPAD_UP
    20,  // KEYCODE_DPAD_DOWN
    21,  // KEYCODE_DPAD_LEFT
    22,  // KEYCODE_DPAD_RIGHT
    23,  // KEYCODE_DPAD_CENTER
    61,  // KEYCODE_TAB
    62,  // KEYCODE_SPACE
    66,  // KEYCODE_ENTER
    67,  // KEYCODE_DEL
    92,  // KEYCODE_PAGE_UP
    93,  // KEYCODE_PAGE_DOWN
    111, // KEYCODE_ESCAPE
];

pub fn validate_serial(serial: &str) -> Result<()> {
    if serial.is_empty() {
        return Err(invalid_argument("serial is empty"));
    }
    if serial.len() > MAX_SERIAL_BYTES {
        return Err(invalid_argument("serial is too long"));
    }
    if serial.starts_with('-') {
        return Err(invalid_argument("serial must not start with '-'"));
    }
    if !serial
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        return Err(invalid_argument(
            "serial contains characters that are not allowed",
        ));
    }
    Ok(())
}

pub fn validate_user(user: u32) -> Result<()> {
    if user > MAX_USER {
        return Err(invalid_argument(format!(
            "Android user must be between 0 and {MAX_USER}"
        )));
    }
    Ok(())
}

pub fn validate_display_size(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(invalid_argument("display dimensions must be positive"));
    }
    if width > MAX_DISPLAY_EDGE || height > MAX_DISPLAY_EDGE {
        return Err(invalid_argument(format!(
            "display edges must be at most {MAX_DISPLAY_EDGE} pixels"
        )));
    }
    if width.saturating_mul(height) > MAX_DISPLAY_PIXELS {
        return Err(invalid_argument(format!(
            "display must contain at most {MAX_DISPLAY_PIXELS} pixels"
        )));
    }
    Ok(())
}

/// Accepts `pkg/.Class`, `pkg/pkg.Class`, and nested classes such as
/// `pkg/pkg.Outer$Inner`. Returns the canonical `pkg/pkg.Class` spelling.
pub fn canonical_component(input: &str) -> Result<String> {
    let Some((package, class)) = input.split_once('/') else {
        return Err(invalid_argument(
            "component must be PACKAGE/CLASS, e.g. com.example/.Main",
        ));
    };
    validate_package(package)?;
    if class.is_empty() {
        return Err(invalid_argument("component class is empty"));
    }
    let canonical_class = if class.starts_with('.') {
        format!("{package}{class}")
    } else if class.starts_with(&format!("{package}.")) {
        class.to_owned()
    } else {
        return Err(invalid_argument(
            "component class must be a relative name or start with the package name",
        ));
    };
    validate_class(&canonical_class, &format!("{package}."))?;
    Ok(format!("{package}/{canonical_class}"))
}

fn validate_package(package: &str) -> Result<()> {
    if package.is_empty() || package.len() > 255 {
        return Err(invalid_argument("component package is empty or too long"));
    }
    for segment in package.split('.') {
        let mut bytes = segment.bytes();
        let Some(first) = bytes.next() else {
            return Err(invalid_argument("component package has an empty segment"));
        };
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return Err(invalid_argument(
                "component package segments must start with a letter or underscore",
            ));
        }
        if !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
            return Err(invalid_argument(
                "component package contains characters that are not allowed",
            ));
        }
    }
    Ok(())
}

fn validate_class(class: &str, prefix: &str) -> Result<()> {
    if class.len() > 255 {
        return Err(invalid_argument("component class is too long"));
    }
    let Some(suffix) = class.strip_prefix(prefix) else {
        return Err(invalid_argument("component class is outside its package"));
    };
    if suffix.is_empty() {
        return Err(invalid_argument("component class name is empty"));
    }
    for segment in suffix.split('.') {
        let mut bytes = segment.bytes();
        let Some(first) = bytes.next() else {
            return Err(invalid_argument("component class has an empty segment"));
        };
        if !(first.is_ascii_alphabetic() || matches!(first, b'_' | b'$')) {
            return Err(invalid_argument(
                "component class segments must start with a letter, underscore, or dollar sign",
            ));
        }
        if !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')) {
            return Err(invalid_argument(
                "component class contains characters that are not allowed",
            ));
        }
    }
    Ok(())
}

pub fn package_of(component: &str) -> &str {
    component
        .split_once('/')
        .map_or(component, |(package, _)| package)
}

pub fn validate_keycode(keycode: u32) -> Result<()> {
    if SAFE_KEYCODES.contains(&keycode) {
        return Ok(());
    }
    Err(invalid_argument(format!(
        "keycode {keycode} is not in the documented safe allowlist {SAFE_KEYCODES:?}"
    )))
}

pub fn validate_coordinates(width: u32, height: u32, points: &[(u32, u32)]) -> Result<()> {
    validate_display_size(width, height)?;
    for (x, y) in points {
        if *x >= width {
            return Err(invalid_argument(format!(
                "x coordinate {x} must be less than the width {width}"
            )));
        }
        if *y >= height {
            return Err(invalid_argument(format!(
                "y coordinate {y} must be less than the height {height}"
            )));
        }
    }
    Ok(())
}

pub fn validate_duration(duration_ms: u32) -> Result<()> {
    if !(1..=MAX_SWIPE_MS).contains(&duration_ms) {
        return Err(invalid_argument(format!(
            "swipe duration must be between 1 and {MAX_SWIPE_MS} ms"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_relative_and_full_components() {
        assert_eq!(
            canonical_component("com.example.app/.Main").unwrap(),
            "com.example.app/com.example.app.Main"
        );
        assert_eq!(
            canonical_component("com.example.app/com.example.app.Main").unwrap(),
            "com.example.app/com.example.app.Main"
        );
        assert_eq!(
            canonical_component("com.example.app/.Outer$Inner").unwrap(),
            "com.example.app/com.example.app.Outer$Inner"
        );
    }

    #[test]
    fn rejects_shell_metacharacters_and_foreign_classes() {
        for bad in [
            "com.example.app/.Main; rm -rf /",
            "com.example.app/../Other",
            "com.example.app/",
            "/Main",
            "com.example.app/Other.Main",
            "com..example/.Main",
            "com.example.app/.Main\n",
            "com.example.app/.Main`id`",
        ] {
            let error = canonical_component(bad).expect_err(bad);
            assert_eq!(error.code, crate::error::ErrorCode::InvalidArgument);
        }
    }

    #[test]
    fn keycode_allowlist_is_small_and_safe() {
        for code in SAFE_KEYCODES {
            validate_keycode(code).unwrap();
        }
        for code in [3, 24, 25, 26, 82, 187] {
            assert!(validate_keycode(code).is_err(), "{code}");
        }
    }

    #[test]
    fn serial_rejects_option_like_values() {
        validate_serial("23049RAD8C").unwrap();
        validate_serial("192.168.1.5:5555").unwrap();
        for bad in ["", "-s", "a b", "a/b", "a;b"] {
            assert!(validate_serial(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn coordinates_are_half_open() {
        validate_coordinates(100, 200, &[(0, 0), (99, 199)]).unwrap();
        assert!(validate_coordinates(100, 200, &[(100, 0)]).is_err());
        assert!(validate_coordinates(100, 200, &[(0, 200)]).is_err());
    }
}
