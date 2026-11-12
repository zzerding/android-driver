use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{DriverError, ErrorCode, Result};

#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(DriverError::new(
                ErrorCode::Cancelled,
                "operation cancelled because the protocol input closed",
            ))
        } else {
            Ok(())
        }
    }
}
