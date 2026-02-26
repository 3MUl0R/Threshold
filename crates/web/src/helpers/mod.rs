pub mod jsonl;
pub mod time;

use crate::error::WebError;
use crate::state::AppState;

/// Check that the daemon is not draining. Returns `Err(DaemonDraining)` if it is.
pub fn check_not_draining(state: &AppState) -> Result<(), WebError> {
    if let Some(ds) = &state.daemon_state {
        if ds.is_draining() {
            return Err(WebError::DaemonDraining);
        }
    }
    Ok(())
}
