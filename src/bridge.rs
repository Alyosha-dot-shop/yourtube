//! A tiny local HTTP proxy that forwards into a SOCKS5 proxy (Tor), for mpv.
//!
//! mpv (ffmpeg) only speaks HTTP proxies, so with Tor it would fetch the video itself from your
//! own address. This listens on 127.0.0.1, accepts `CONNECT host:443` for YouTube's servers
//! only, and opens that connection through the SOCKS proxy. yt-dlp (inside mpv) uses the bridge
//! too, so finding the stream and fetching it come out of the same Tor exit (YouTube's stream
//! links only work from the address that asked for them).

use anyhow::{Result, anyhow, bail};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Hosts yt-dlp and mpv may reach through the bridge: YouTube's own.
fn allowed(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h == "youtube.com"
        || h == "youtubei.googleapis.com"
        || [".youtube.com", ".googlevideo.com", ".ytimg.com", ".ggpht.com"].iter().any(|s| h.ends_with(s))
}

/// `socks5h://user:pass@host:port` → (address, user, pass).
fn parse_socks(proxy: &str) -> Result<(String, String, String)> {
    let (scheme, rest) = proxy.split_once("://").ok_or_else(|| anyhow!("not a proxy URL"))?;
    if !scheme.eq_ignore_ascii_case("socks5h") && !scheme.eq_ignore_ascii_case("socks5") {
        bail!("only SOCKS5 proxies can be bridged");
    }
    let rest = rest.trim_end_matches('/');
    let (auth, addr) = rest.rsplit_once('@').map_or(("", rest), |(a, h)| (a, h));
    let (user, pass) = auth.split_once(':').unwrap_or((auth, ""));
    Ok((addr.to_owned(), user.to_owned(), pass.to_owned()))
}

/// Start (once per proxy) and return the bridge's address, e.g. `http://127.0.0.1:41234`.
pub fn http_proxy_for(socks: &str) -> Result<String> {
    static BRIDGES: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();
    let bridges = BRIDGES.get_or_init(Mutex::default);
    let mut list = bridges.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, url)) = list.iter().find(|(s, _)| s == socks) {
        return Ok(url.clone());
    }
    let target = parse_socks(socks)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let url = format!("http://{}", listener.local_addr()?);
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let target = target.clone();
            std::thread::spawn(move || {
                let _ = serve(conn, &target);
            });
        }
    });
    list.push((socks.to_owned(), url.clone()));
    Ok(url)
}

fn serve(client: TcpStream, (addr, user, pass): &(String, String, String)) -> Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(client.try_clone()?);
    let mut line = String::new();
    reader.by_ref().take(8192).read_line(&mut line)?;
    // Skip the headers (ffmpeg sends a few); stop at the blank line.
    for _ in 0..64 {
        let mut h = String::new();
        if reader.by_ref().take(8192).read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
    }
    let mut client = client;
    let mut parts = line.split_whitespace();
    let (Some("CONNECT"), Some(dest)) = (parts.next(), parts.next()) else {
        client.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n")?;
        return Ok(());
    };
    let (host, port) = dest.rsplit_once(':').ok_or_else(|| anyhow!("no port"))?;
    let port: u16 = port.parse()?;
    if port != 443 || !allowed(host) {
        client.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")?;
        return Ok(());
    }
    let upstream = match socks5_connect(addr, user, pass, host, port) {
        Ok(s) => s,
        Err(e) => {
            client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")?;
            return Err(e);
        }
    };
    client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
    client.set_read_timeout(None)?;
    // Anything ffmpeg sent after the headers (the TLS hello) is still in the reader's buffer.
    let pending = reader.buffer().to_vec();
    let (mut up_r, mut up_w) = (upstream.try_clone()?, upstream);
    up_w.write_all(&pending)?;
    let mut client_r = client.try_clone()?;
    let t = std::thread::spawn(move || {
        let _ = std::io::copy(&mut client_r, &mut up_w);
        let _ = up_w.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut up_r, &mut client);
    let _ = client.shutdown(std::net::Shutdown::Both);
    let _ = t.join();
    Ok(())
}

/// SOCKS5 CONNECT by name (the proxy resolves it, so no DNS leaves this machine).
fn socks5_connect(proxy: &str, user: &str, pass: &str, host: &str, port: u16) -> Result<TcpStream> {
    let addr = proxy.to_socket_addrs()?.next().ok_or_else(|| anyhow!("proxy address"))?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(10))?;
    s.set_read_timeout(Some(Duration::from_secs(60)))?;
    let with_auth = !user.is_empty();
    s.write_all(if with_auth { &[5, 1, 2] } else { &[5, 1, 0] })?;
    let mut r = [0u8; 2];
    s.read_exact(&mut r)?;
    match (r[1], with_auth) {
        (0, _) => {}
        (2, true) => {
            let (u, p) = (user.as_bytes(), pass.as_bytes());
            anyhow::ensure!(u.len() < 256 && p.len() < 256, "credentials too long");
            let mut m = vec![1, u.len() as u8];
            m.extend(u);
            m.push(p.len() as u8);
            m.extend(p);
            s.write_all(&m)?;
            s.read_exact(&mut r)?;
            anyhow::ensure!(r[1] == 0, "proxy refused the login");
        }
        _ => bail!("proxy wants an unsupported login"),
    }
    let h = host.as_bytes();
    anyhow::ensure!(h.len() < 256, "host too long");
    let mut m = vec![5, 1, 0, 3, h.len() as u8];
    m.extend(h);
    m.extend(port.to_be_bytes());
    s.write_all(&m)?;
    let mut head = [0u8; 4];
    s.read_exact(&mut head)?;
    anyhow::ensure!(head[1] == 0, "proxy couldn't connect (code {})", head[1]);
    let skip = match head[3] {
        1 => 4 + 2,
        4 => 16 + 2,
        3 => {
            let mut n = [0u8; 1];
            s.read_exact(&mut n)?;
            n[0] as usize + 2
        }
        _ => bail!("bad proxy reply"),
    };
    let mut rest = vec![0u8; skip];
    s.read_exact(&mut rest)?;
    s.set_read_timeout(None)?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proxies_and_limits_hosts() {
        assert_eq!(
            parse_socks("socks5h://yt-play-ab:x@127.0.0.1:9050").unwrap(),
            ("127.0.0.1:9050".into(), "yt-play-ab".into(), "x".into())
        );
        assert_eq!(parse_socks("socks5://127.0.0.1:9050").unwrap(), ("127.0.0.1:9050".into(), String::new(), String::new()));
        assert!(parse_socks("http://p:3128").is_err());
        assert!(allowed("rr3---sn-abc.googlevideo.com") && allowed("i.ytimg.com"));
        assert!(!allowed("googlevideo.com.evil.example") && !allowed("example.com"));
    }

    #[test]
    fn refuses_other_hosts_and_plain_requests() {
        let url = http_proxy_for("socks5h://u:p@127.0.0.1:1").unwrap();
        let addr = url.trim_start_matches("http://");
        for (req, want) in [
            ("CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\n\r\n", "403"),
            ("CONNECT rr1.googlevideo.com:80 HTTP/1.1\r\n\r\n", "403"),
            ("GET http://example.com/ HTTP/1.1\r\n\r\n", "405"),
            ("CONNECT rr1.googlevideo.com:443 HTTP/1.1\r\n\r\n", "502"), // nothing listens on :1
        ] {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(req.as_bytes()).unwrap();
            let mut out = String::new();
            let _ = s.read_to_string(&mut out);
            assert!(out.starts_with(&format!("HTTP/1.1 {want}")), "{req:?} → {out:?}");
        }
    }
}
