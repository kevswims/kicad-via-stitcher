//! Connection setup and the one KiCad error mode that needs special handling.

use anyhow::{Context, Result};
use kicad_ipc_rs::{KiCadClientBlocking, KiCadError};
use std::time::Duration;

/// Well above the library's 3s default: bulk reads and creates on a large
/// board routinely take longer than that.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

const BUSY_RETRIES: u32 = 3;
const BUSY_BACKOFF: Duration = Duration::from_millis(400);

pub fn connect() -> Result<KiCadClientBlocking> {
    KiCadClientBlocking::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect()
        .context("connecting to KiCad — is it running with Preferences > Plugins > Enable IPC API?")
}

/// True when KiCad refused the command because the editor is busy. This is
/// persistent, not transient, while a modal dialog is open: KiCad answers
/// `GetVersion` and `GetNets` happily but refuses document commands until the
/// dialog is dismissed.
pub fn is_busy(err: &KiCadError) -> bool {
    matches!(err, KiCadError::ApiStatus { code, .. } if code == "AS_BUSY")
}

/// Runs `op`, retrying briefly if KiCad reports itself busy. A genuinely
/// transient busy state (an in-flight zone refill, a drag in progress) clears
/// on its own; an open dialog does not, so the retries are few and the error
/// that escapes says what to do about it.
pub fn with_busy_retry<T>(what: &str, mut op: impl FnMut() -> Result<T, KiCadError>) -> Result<T> {
    let mut attempt = 0;
    loop {
        match op() {
            Ok(value) => return Ok(value),
            Err(err) if is_busy(&err) && attempt < BUSY_RETRIES => {
                attempt += 1;
                std::thread::sleep(BUSY_BACKOFF * attempt);
            }
            Err(err) if is_busy(&err) => {
                return Err(anyhow::anyhow!(
                    "KiCad is busy and will not answer `{what}`.\n\
                     This usually means a dialog is open in the PCB editor — close it and re-run.\n\
                     (It also happens mid-drag, or while a zone refill is running.)"
                ));
            }
            Err(err) => return Err(anyhow::Error::new(err).context(format!("KiCad call `{what}`"))),
        }
    }
}
