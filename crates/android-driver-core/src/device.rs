#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub serial: String,
    pub state: String,
    pub model: Option<String>,
    pub product: Option<String>,
    pub device: Option<String>,
}

/// Parses `adb devices -l`. Every non-empty entry line must have a serial,
/// a state, and only key:value attributes afterwards; an unfamiliar line is
/// an error instead of a silently skipped device.
pub fn parse_devices(output: &str) -> std::result::Result<Vec<Device>, String> {
    let mut lines = output.lines();
    let header = loop {
        match lines.next() {
            Some(line) if line.trim().is_empty() => continue,
            Some(line) => break line,
            None => return Err("adb devices returned no output".to_owned()),
        }
    };
    if header.trim() != "List of devices attached" {
        return Err(format!("unexpected adb devices header: {header}"));
    }

    let mut devices = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let serial = parts
            .next()
            .ok_or_else(|| format!("device line is missing a serial: {line}"))?;
        let state = parts
            .next()
            .ok_or_else(|| format!("device line is missing a state: {line}"))?;
        let mut device = Device {
            serial: serial.to_owned(),
            state: state.to_owned(),
            model: None,
            product: None,
            device: None,
        };
        for attribute in parts {
            let Some((key, value)) = attribute.split_once(':') else {
                return Err(format!("device line has an unknown field: {line}"));
            };
            let slot = match key {
                "model" => &mut device.model,
                "product" => &mut device.product,
                "device" => &mut device.device,
                "transport_id" => continue,
                _ => return Err(format!("device line has an unknown field: {line}")),
            };
            *slot = Some(value.to_owned());
        }
        devices.push(device);
    }
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_short_and_long_forms() {
        let devices = parse_devices(
            "List of devices attached\nFAKE0001\tdevice product:fake model:Fake_Phone device:fake transport_id:1\n192.168.1.9:5555\toffline\n",
        )
        .unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].serial, "FAKE0001");
        assert_eq!(devices[0].state, "device");
        assert_eq!(devices[0].model.as_deref(), Some("Fake_Phone"));
        assert_eq!(devices[1].state, "offline");
        assert_eq!(devices[1].model, None);
    }

    #[test]
    fn rejects_unfamiliar_output() {
        assert!(parse_devices("error: no server\n").is_err());
        assert!(parse_devices("List of devices attached\nserial device mystery\n").is_err());
        assert!(parse_devices("List of devices attached\nserial device model:\n").is_ok());
    }
}
