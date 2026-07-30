//! OWNER: unit P-04 (providers/src/vast/{rent,boot,stall}.rs). Do not edit outside that
//! unit.
//!
//! Download-stall detection **and recovery** — the inventory marks this
//! "genuinely valuable — keep it", so mk1 keeps both halves.
//!
//! Detection is a 4-second `/proc/net/dev` eth0 RX delta over ssh: `< 1000 bytes` is
//! stalled, `< 50 Mbps` is slow. Recovery pkills `launch.sh` + `hf download`, recovers the
//! environment from `/proc/<pid>/environ`, **forces `HOST=127.0.0.1`**, and re-execs with
//! `>>` (append), never `>`.

use apexrouter_core::error::Result;
use apexrouter_core::exec::SshOpts;
use apexrouter_protocol::DownloadHealth;

/// Sample the instance's inbound traffic over 4 seconds.
pub async fn sample_download(ssh: &SshOpts, host: &str, port: u16) -> Result<DownloadHealth> {
    todo!("P-04: sample_download")
}

/// The one-click recovery behind the stall banner.
pub async fn restart_download(ssh: &SshOpts, host: &str, port: u16) -> Result<()> {
    todo!("P-04: restart_download")
}
