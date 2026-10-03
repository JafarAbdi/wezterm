//! Test fixture: holds a connection attempt at one step so the reconnect
//! suite can cancel it there.
//!
//! `sshmux_stall listen <address> <port>` binds a listener whose accept
//! queue it fills itself, so the kernel drops every further SYN and a
//! client's TCP connect stays in progress.  It runs until it is killed.
//!
//! `sshmux_stall version <log>` and `sshmux_stall list <log>` stand in for
//! `wezterm cli proxy`: `version` answers nothing, `list` answers the codec
//! version and the client id but never the pane list.  Each appends what
//! it waits on to `<log>` and exits at end of input.  Neither serves a pane
//! or starts anything.

use codec::{CODEC_VERSION, GetCodecVersionResponse, Pdu, UnitResponse};
use std::io::Write;
use std::net::SocketAddr;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["listen", address, port] => listen(format!("{address}:{port}").parse()?),
        ["version", log] => proxy(log, false),
        ["list", log] => proxy(log, true),
        _ => anyhow::bail!(
            "usage: sshmux_stall listen <address> <port> | version <log> | list <log>"
        ),
    }
}

fn listen(addr: SocketAddr) -> anyhow::Result<()> {
    let listener = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        None,
    )?;
    listener.set_reuse_address(true)?;
    listener.bind(&addr.into())?;
    listener.listen(0)?;
    let _queued = std::net::TcpStream::connect(addr)?;
    println!("listening {addr} with a full accept queue");
    std::io::stdout().flush()?;
    loop {
        std::thread::park();
    }
}

fn proxy(log: &str, answer_version: bool) -> anyhow::Result<()> {
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    writeln!(log, "start pid={}", std::process::id())?;
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    while let Ok(request) = Pdu::decode(&mut stdin) {
        let response = match &request.pdu {
            Pdu::GetCodecVersion(_) if answer_version => {
                Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                    codec_vers: CODEC_VERSION,
                    version_string: "fixture-stall-list".to_string(),
                    executable_path: std::env::current_exe()?,
                    config_file_path: None,
                })
            }
            Pdu::SetClientId(_) if answer_version => Pdu::UnitResponse(UnitResponse {}),
            pdu => {
                writeln!(log, "waiting {}", pdu.pdu_name())?;
                continue;
            }
        };
        response.encode(&mut stdout, request.serial)?;
        stdout.flush()?;
    }
    writeln!(log, "eof")?;
    Ok(())
}
