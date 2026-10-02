//! Test fixture: stands in for `wezterm cli proxy` on a laptop whose
//! wezterm speaks another mux protocol version.
//!
//! It answers every request on stdin with a `GetCodecVersionResponse`
//! whose codec version is one above this build's and exits at end of
//! input.  It serves no panes and starts nothing.

use codec::{CODEC_VERSION, GetCodecVersionResponse, Pdu};
use std::io::Write;

fn main() -> anyhow::Result<()> {
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    while let Ok(request) = Pdu::decode(&mut stdin) {
        let response = Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
            codec_vers: CODEC_VERSION + 1,
            version_string: "fixture-codec-mismatch".to_string(),
            executable_path: std::env::current_exe()?,
            config_file_path: None,
        });
        response.encode(&mut stdout, request.serial)?;
        stdout.flush()?;
    }
    Ok(())
}
