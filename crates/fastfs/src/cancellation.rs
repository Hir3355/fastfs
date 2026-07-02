use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(crate) type CancellationState = Arc<AtomicBool>;

/// Opaque cancellation token shared with the PowerShell bridge.
pub struct FastFsCancellationToken {
    state: CancellationState,
}

impl FastFsCancellationToken {
    pub(crate) fn state(&self) -> CancellationState {
        Arc::clone(&self.state)
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn fastfs_cancellation_create() -> *mut FastFsCancellationToken {
    Box::into_raw(Box::new(FastFsCancellationToken {
        state: Arc::new(AtomicBool::new(false)),
    }))
}

/// Requests cancellation for an in-flight FastFs operation.
///
/// # Safety
///
/// `token` must be null or a live pointer returned by
/// [`fastfs_cancellation_create`]. The caller must prevent concurrent
/// destruction while this function is running.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn fastfs_cancellation_cancel(token: *const FastFsCancellationToken) {
    // SAFETY: The caller guarantees that a non-null token remains live for this call.
    if let Some(token) = unsafe { token.as_ref() } {
        token.state.store(true, Ordering::Relaxed);
    }
}

/// Releases a cancellation token.
///
/// # Safety
///
/// `token` must be null or a live pointer returned by
/// [`fastfs_cancellation_create`] that has not already been destroyed. No
/// cancellation or execution call may concurrently access the token.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn fastfs_cancellation_destroy(token: *mut FastFsCancellationToken) {
    if !token.is_null() {
        // SAFETY: The caller transfers the unique allocation back to Rust.
        drop(unsafe { Box::from_raw(token) });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::{
        fastfs_cancellation_cancel, fastfs_cancellation_create, fastfs_cancellation_destroy,
    };

    #[test]
    fn ffi_token_shares_cancellation_with_active_operations() {
        let token = fastfs_cancellation_create();
        assert!(!token.is_null());

        // SAFETY: The token is live until it is destroyed at the end of the test.
        let state = unsafe { (*token).state() };
        assert!(!state.load(Ordering::Relaxed));

        // SAFETY: The token remains live during this call.
        unsafe { fastfs_cancellation_cancel(token) };
        assert!(state.load(Ordering::Relaxed));

        // SAFETY: This is the token's only destruction.
        unsafe { fastfs_cancellation_destroy(token) };
        assert!(state.load(Ordering::Relaxed));
    }
}
