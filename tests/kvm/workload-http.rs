use std::{
    io::{Read, Write},
    net::{TcpListener, ToSocketAddrs},
    time::Duration,
};

unsafe extern "C" {
    fn signal(number: i32, handler: extern "C" fn(i32)) -> usize;
    fn _exit(status: i32) -> !;
}

extern "C" fn terminate(_: i32) {
    unsafe { _exit(0) }
}

fn main() -> std::io::Result<()> {
    let mode = std::env::args().nth(1);
    let body = match mode.as_deref() {
        Some("resolve") => {
            let authority = std::env::args().nth(2).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "expected DNS name:port")
            })?;
            let addresses = authority.to_socket_addrs()?.collect::<Vec<_>>();
            if addresses.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "DNS returned no addresses",
                ));
            }
            for address in addresses {
                println!("{address}");
            }
            return Ok(());
        }
        Some("serve-hostname") => format!(
            "podmesh-kvm-response:{}",
            std::env::var("HOSTNAME")
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?
        ),
        Some(url) => return fetch(url),
        None => "podmesh-kvm-response".to_string(),
    };
    // Container PID 1 ignores default SIGTERM; explicitly exit on Podman stop.
    if unsafe { signal(15, terminate) } == usize::MAX {
        return Err(std::io::Error::last_os_error());
    }
    let port = match std::env::var("PODMESH_KVM_HTTP_PORT") {
        Ok(port) => port
            .parse::<u16>()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?,
        Err(std::env::VarError::NotPresent) => 8081,
        Err(error) => {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, error));
        }
    };
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    for connection in listener.incoming() {
        let mut connection = connection?;
        connection.set_read_timeout(Some(Duration::from_secs(5)))?;
        connection.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut request = [0; 4096];
        let _ = connection.read(&mut request)?;
        write!(
            connection,
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
    }
    Ok(())
}

fn fetch(url: &str) -> std::io::Result<()> {
    let url = url.strip_prefix("http://").ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "expected http URL")
    })?;
    let (authority, path) = url.split_once('/').unwrap_or((url, ""));
    let address = authority.to_socket_addrs()?.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "URL has no address")
    })?;
    let timeout = Duration::from_secs(5);
    let mut stream = std::net::TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid HTTP"))?;
    if !response.starts_with(b"HTTP/1.1 200 ") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unexpected HTTP response status",
        ));
    }
    std::io::stdout().write_all(&response[header_end + 4..])
}
