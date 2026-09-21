//! nettest : essais du reseau depuis une application ordinaire.
//!
//!     nettest info                  configuration (DHCP)
//!     nettest resolve NOM           resolution DNS
//!     nettest get HOTE PORT CHEMIN  requete HTTP/1.0, affiche la reponse
//!     nettest serve PORT [N]        sert N requetes HTTP (page de test)
//!     nettest udp HOTE PORT TEXTE   datagramme UDP et attente d'une reponse

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;

use libnx::abi::err;
use libnx::net::{self, fmt_ipv4, TcpListener, TcpStream, UdpSocket};
use libnx::{console, env, println, time};

libnx::entry!(main);

fn host(name: &str) -> Result<u32, i64> {
    net::resolve(name)
}

fn main() -> i32 {
    let args: Vec<&str> = env::args().collect();
    let r = match args.get(1).copied() {
        Some("info") => net::info().map(|(a, p, g, d)| {
            println!("adresse {}/{}, passerelle {}, DNS {}", fmt_ipv4(a), p, fmt_ipv4(g), fmt_ipv4(d));
        }),
        Some("resolve") if args.len() == 3 => host(args[2]).map(|a| println!("{} -> {}", args[2], fmt_ipv4(a))),
        Some("get") if args.len() == 5 => get(args[2], args[3].parse().unwrap_or(80), args[4]),
        Some("serve") if args.len() >= 3 => serve(args[2].parse().unwrap_or(80), args.get(3).and_then(|n| n.parse().ok()).unwrap_or(1)),
        Some("udp") if args.len() == 5 => udp(args[2], args[3].parse().unwrap_or(7), args[4]),
        _ => {
            println!("usage : nettest info | resolve NOM | get HOTE PORT CHEMIN | serve PORT [N] | udp HOTE PORT TEXTE");
            return 2;
        }
    };
    match r {
        Ok(()) => 0,
        Err(e) => {
            println!("nettest : {}", err::name(e));
            1
        }
    }
}

fn get(h: &str, port: u16, path: &str) -> Result<(), i64> {
    let addr = host(h)?;
    let t0 = time::now_ns();
    let mut s = TcpStream::connect(addr, port, 5_000_000_000)?;
    s.set_timeout(10_000_000_000);
    s.write_all(format!("GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: nxk-nettest\r\n\r\n", path, h).as_bytes())?;
    let mut total = 0;
    let mut buf = [0u8; 4096];
    loop {
        let n = s.read(&mut buf)?;
        if n == 0 {
            break;
        }
        console::write(&buf[..n]);
        total += n;
    }
    println!("\n[{} octets recus en {} ms]", total, (time::now_ns() - t0) / 1_000_000);
    Ok(())
}

fn serve(port: u16, n: u32) -> Result<(), i64> {
    let l = TcpListener::bind(port, 4)?;
    println!("nettest : ecoute sur le port {}", port);
    for k in 0..n {
        let (mut s, a, p) = l.accept(0)?;
        s.set_timeout(5_000_000_000);
        let mut req = [0u8; 2048];
        let n = s.read(&mut req)?;
        let line = core::str::from_utf8(&req[..n]).unwrap_or("").lines().next().unwrap_or("");
        println!("nettest : requete {} de {}:{} : {}", k + 1, fmt_ipv4(a), p, line);
        let body = format!("Bonjour depuis nxk ! Requete numero {} : {}\n", k + 1, line);
        s.write_all(
            format!("HTTP/1.0 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}", body.len(), body)
                .as_bytes(),
        )?;
        s.shutdown()?;
        // Laisse le client lire et fermer de son cote.
        let mut sink = [0u8; 64];
        let _ = s.read(&mut sink);
    }
    Ok(())
}

fn udp(h: &str, port: u16, text: &str) -> Result<(), i64> {
    let addr = host(h)?;
    let sock = UdpSocket::bind(0)?;
    sock.send_to(text.as_bytes(), addr, port)?;
    let mut buf = [0u8; 2048];
    let (n, a, p) = sock.recv_from(&mut buf, 3_000_000_000)?;
    println!("reponse de {}:{} : {}", fmt_ipv4(a), p, core::str::from_utf8(&buf[..n]).unwrap_or("?"));
    Ok(())
}
