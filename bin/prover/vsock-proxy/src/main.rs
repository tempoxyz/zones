use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::process::ExitCode;

#[cfg(target_os = "linux")]
use std::mem::size_of;
#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;

use clap::Parser;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio_vsock::VsockStream;

#[cfg(target_os = "linux")]
const VMADDR_FLAG_TO_HOST: u8 = 1;

#[cfg(target_os = "linux")]
#[repr(C)]
struct SockAddrVm {
    family: libc::sa_family_t,
    reserved: libc::c_ushort,
    port: libc::c_uint,
    cid: libc::c_uint,
    flags: u8,
    zero: [u8; 3],
}

#[derive(Debug, Parser, PartialEq)]
#[command(
    version,
    about = "Forward TCP connections to a VSOCK endpoint via the host transport"
)]
struct Config {
    /// TCP port on which to accept connections.
    #[arg(value_name = "TCP_PORT")]
    listen_port: u16,

    /// Destination VSOCK context identifier.
    #[arg(value_name = "VSOCK_CID")]
    vsock_cid: u32,

    /// Destination VSOCK port.
    #[arg(value_name = "VSOCK_PORT")]
    vsock_port: u32,
}

#[tokio::main]
async fn main() -> ExitCode {
    let config = Config::parse();

    match run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tempo-vsock-proxy failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(config: Config) -> io::Result<()> {
    let listen_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, config.listen_port);
    let listener = TcpListener::bind(listen_addr).await?;

    println!(
        "Listening on TCP {} and forwarding to VSOCK CID {}, port {} with VMADDR_FLAG_TO_HOST",
        listen_addr, config.vsock_cid, config.vsock_port
    );

    loop {
        let (tcp, peer) = listener.accept().await?;
        let cid = config.vsock_cid;
        let port = config.vsock_port;

        tokio::spawn(async move {
            if let Err(error) = proxy_connection(tcp, cid, port).await {
                eprintln!("connection from {peer} failed: {error}");
            }
        });
    }
}

async fn proxy_connection(mut tcp: TcpStream, cid: u32, port: u32) -> io::Result<()> {
    let mut vsock = connect_to_host(cid, port).await?;
    copy_bidirectional(&mut tcp, &mut vsock).await?;
    Ok(())
}

async fn connect_to_host(cid: u32, port: u32) -> io::Result<VsockStream> {
    let connected = tokio::task::spawn_blocking(move || connect_blocking(cid, port))
        .await
        .map_err(io::Error::other)??;
    VsockStream::new(connected)
}

#[cfg(target_os = "linux")]
fn connect_blocking(cid: u32, port: u32) -> io::Result<vsock::VsockStream> {
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let addr = SockAddrVm {
        family: libc::AF_VSOCK as libc::sa_family_t,
        reserved: 0,
        port,
        cid,
        flags: VMADDR_FLAG_TO_HOST,
        zero: [0; 3],
    };

    let result = unsafe {
        libc::connect(
            fd,
            &addr as *const SockAddrVm as *const libc::sockaddr,
            size_of::<SockAddrVm>() as libc::socklen_t,
        )
    };

    if result < 0 {
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(fd);
        }
        return Err(error);
    }

    Ok(unsafe { vsock::VsockStream::from_raw_fd(fd) })
}

#[cfg(not(target_os = "linux"))]
fn connect_blocking(_cid: u32, _port: u32) -> io::Result<vsock::VsockStream> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "VMADDR_FLAG_TO_HOST is supported only on Linux",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config() {
        let config = Config::try_parse_from(["proxy", "5000", "16", "5001"]).unwrap();

        assert_eq!(
            config,
            Config {
                listen_port: 5000,
                vsock_cid: 16,
                vsock_port: 5001,
            }
        );
    }

    #[test]
    fn rejects_invalid_config() {
        let error = Config::try_parse_from(["proxy", "not-a-port", "16", "5000"]).unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn socket_address_matches_linux_abi() {
        assert_eq!(size_of::<SockAddrVm>(), size_of::<libc::sockaddr_vm>());
        assert_eq!(std::mem::offset_of!(SockAddrVm, flags), 12);

        let addr = SockAddrVm {
            family: libc::AF_VSOCK as libc::sa_family_t,
            reserved: 0,
            port: 5000,
            cid: 16,
            flags: VMADDR_FLAG_TO_HOST,
            zero: [0; 3],
        };

        assert_eq!(addr.flags, VMADDR_FLAG_TO_HOST);
    }
}
