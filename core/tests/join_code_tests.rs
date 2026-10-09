use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use feverlink3_core::crypto::SessionKey;
use feverlink3_core::lobby::{base32_decode, base32_encode, JoinCode};

const SECRET: [u8; 16] = {
    let mut s = [0u8; 16];
    let mut i = 0;
    while i < 16 {
        s[i] = 100 + i as u8;
        i += 1;
    }
    s
};

fn ep(ip: &str, port: u16) -> SocketAddr {
    SocketAddr::new(ip.parse::<IpAddr>().unwrap(), port)
}

const LOOPBACK: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);

#[test]
fn code_round_trips_with_ipv4_and_ipv6_endpoints() {
    let eps = [ep("192.168.1.20", 28090), ep("2001:db8::1", 28091)];
    // 2026-10-09 12:00:00 UTC
    let code = JoinCode::create(&eps, SECRET, Some(1_791_547_200)).unwrap();
    let text = code.encode();
    let back = JoinCode::try_decode(&text).expect("decodes");
    assert_eq!(code, back);
    assert_eq!(2, back.endpoints.len());
    assert_eq!(28091, back.endpoints[1].port());
}

#[test]
fn one_ipv4_endpoint_is_53_characters() {
    let code = JoinCode::create(&[ep("89.12.34.56", 28090)], SECRET, None).unwrap();
    assert_eq!(53, code.encode().len());
}

#[test]
fn typing_variations_are_accepted() {
    let code = JoinCode::create(&[LOOPBACK], SECRET, None).unwrap();
    let text = code.encode();
    let typed: String = text
        .to_lowercase()
        .chars()
        .enumerate()
        .map(|(i, c)| if i % 5 == 4 { format!("{c}-") } else { c.to_string() })
        .collect();
    assert_eq!(Some(code), JoinCode::try_decode(&format!(" {typed} ")));
}

#[test]
fn a_changed_character_is_refused() {
    let text = JoinCode::create(&[LOOPBACK], SECRET, None).unwrap().encode();
    for i in 0..text.len() {
        let c = if &text[i..=i] == "A" { "B" } else { "A" };
        let broken = format!("{}{}{}", &text[..i], c, &text[i + 1..]);
        assert_eq!(None, JoinCode::try_decode(&broken), "position {i}");
    }
}

#[test]
fn garbage_is_none_not_a_panic_whitespace() {
    assert_eq!(None, JoinCode::try_decode(" \t"));
}

#[test]
fn garbage_is_none_not_a_panic_empty() {
    assert_eq!(None, JoinCode::try_decode(""));
}

#[test]
fn garbage_is_none_not_a_panic_not_a_code() {
    assert_eq!(None, JoinCode::try_decode("not a code"));
}

#[test]
fn garbage_is_none_not_a_panic_abcdefg() {
    assert_eq!(None, JoinCode::try_decode("ABCDEFG"));
}

#[test]
fn garbage_is_none_not_a_panic_a1() {
    assert_eq!(None, JoinCode::try_decode("A1"));
}

#[test]
fn secret_feeds_the_session_key() {
    let code = JoinCode::create(&[], SECRET, None).unwrap();
    let back = JoinCode::try_decode(&code.encode()).unwrap();
    assert_eq!(SessionKey::derive(&SECRET, None).key(), SessionKey::derive(&back.secret, None).key());
}

#[test]
fn base32_matches_rfc_4648_vectors() {
    assert_eq!("MZXW6YTBOI", base32_encode(b"foobar"));
    assert_eq!(Some(b"foobar".to_vec()), base32_decode("mzxw6ytboi======"));
    assert_eq!("MY", base32_encode(b"f"));
    assert_eq!(None, base32_decode("M1"));
}
