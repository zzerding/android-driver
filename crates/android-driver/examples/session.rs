use std::ffi::OsString;
use std::time::Duration;

use android_driver_core::{AdbEndpoint, CancelToken, Driver, DriverConfig, OpenRequest};

fn main() -> android_driver_core::Result<()> {
    let mut driver = Driver::new(DriverConfig {
        adb_path: OsString::from("adb"),
        endpoint: AdbEndpoint::parse("localhost:5037")?,
        helper_path: Some("helper/build/virtual-display-helper.zip".into()),
        lock_dir: None,
        adb_timeout: Duration::from_secs(15),
        helper_timeout: Duration::from_secs(8),
        cleanup_timeout: Duration::from_secs(5),
        cancel: CancelToken::new(),
    });
    let result = (|| {
        let session = driver.open(OpenRequest {
            serial: "DEVICE_SERIAL".into(),
            user: 0,
        })?;
        driver.launch("com.example.app/.Main")?;
        driver.screenshot()?;
        driver.close()?;
        println!("virtual display {} closed", session.display_id);
        Ok(())
    })();
    for error in driver.shutdown() {
        eprintln!("cleanup: {error}");
    }
    result
}
