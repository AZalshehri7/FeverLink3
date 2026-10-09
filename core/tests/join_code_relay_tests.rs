//! Join codes with relays (version 2), and the promise that codes of version 1 keep working both ways.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use feverlink3_core::crypto::SessionKey;
use feverlink3_core::lobby::{base32_decode, base32_encode, JoinCode, JoinCodeError, RelayAddr};
use sha2::{Digest, Sha256};

const SECRET: [u8; 16] = {
    let mut s = [0u8; 16];
    let mut i = 0;
    while i < 16 {
        s[i] = 100 + i as u8;
        i += 1;
    }
    s
};
const CREATED: u32 = 1_791_547_200;

fn relay(text: &str) -> RelayAddr {
    RelayAddr::parse(text, None).unwrap_or_else(|| panic!("{text} parses"))
}

fn lan() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)), 28090)
}

/// A code body as the encoder would write it, with a valid check, for building shapes the encoder never writes.
fn seal(mut body: Vec<u8>) -> String {
    let check = Sha256::digest(&body);
    body.extend_from_slice(&check[..4]);
    base32_encode(&body)
}

/// Version, created, secret, no endpoints.
fn head(version: u8) -> Vec<u8> {
    let mut b = vec![version];
    b.extend_from_slice(&CREATED.to_be_bytes());
    b.extend_from_slice(&SECRET);
    b.push(0); // endpoint count
    b
}

fn relay_value(entries: &[&[u8]]) -> Vec<u8> {
    let mut v = vec![entries.len() as u8];
    for e in entries {
        v.extend_from_slice(e);
    }
    v
}

fn extension(tag: u8, value: &[u8]) -> Vec<u8> {
    let mut e = vec![tag];
    e.extend_from_slice(&(value.len() as u16).to_be_bytes());
    e.extend_from_slice(value);
    e
}

/// One IPv4 relay entry: family, address, port.
const V4_RELAY: [u8; 7] = [4, 203, 0, 113, 9, 0x6E, 0xE4]; // 203.0.113.9:28388

#[test]
fn relays_round_trip_in_a_version_2_code() {
    let relays = [relay("203.0.113.9:28100"), relay("[2001:db8::7]:28100"), relay("relay.example.org:443")];
    let code = JoinCode::create(&[lan()], SECRET, Some(CREATED)).unwrap().with_relays(&relays).unwrap();
    assert_eq!(code.version, 2);
    let back = JoinCode::try_decode(&code.encode()).expect("decodes");
    assert_eq!(back, code);
    assert_eq!(back.relays, relays);
    assert_eq!(back.endpoints, vec![lan()]);
    assert_eq!(back.version, 2);
    assert_eq!(back.relay_room(), SessionKey::relay_room_id(&SECRET), "the room is derived, never carried");
}

#[test]
fn a_code_without_relays_is_still_written_as_version_1() {
    let code =
        JoinCode::create(&[SocketAddr::new(IpAddr::V4(Ipv4Addr::new(89, 12, 34, 56)), 28090)], SECRET, Some(CREATED))
            .unwrap();
    let text = code.encode();
    assert_eq!(text.len(), 53, "the same 53 characters as before relays existed");
    assert_eq!(base32_decode(&text).unwrap()[0], 1);
    // taking the relays away again gives the old layout back
    let with = code.clone().with_relays(&[relay("203.0.113.9:28100")]).unwrap();
    assert_eq!(base32_decode(&with.encode()).unwrap()[0], 2);
    let without = with.with_relays(&[]).unwrap();
    assert_eq!(without, code);
    assert_eq!(without.encode(), text);
}

#[test]
fn a_version_1_code_made_before_relays_existed_still_decodes() {
    // written by the version 1 encoder (checked against an independent implementation of the layout)
    let old = "AFVMRV2AMRSWMZ3INFVGW3DNNZXXA4LSOMAQIWIMEI4G3OTYL6YBA";
    let code = JoinCode::try_decode(old).expect("an old code still decodes");
    assert_eq!(code.version, 1);
    assert_eq!(code.created, CREATED);
    assert_eq!(code.secret, SECRET);
    assert_eq!(code.endpoints, vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(89, 12, 34, 56)), 28090)]);
    assert!(code.relays.is_empty());
    assert_eq!(code.encode(), old, "and encodes back to the same text");
    let empty = JoinCode::try_decode("AFVMRV2AMRSWMZ3INFVGW3DNNZXXA4LSOMABN5KSSY").expect("no endpoints");
    assert!(empty.endpoints.is_empty() && empty.relays.is_empty());
}

#[test]
fn a_relay_by_name_is_longer_than_a_relay_by_address_but_still_short() {
    let by_ip =
        JoinCode::create(&[], SECRET, Some(CREATED)).unwrap().with_relays(&[relay("203.0.113.9:28100")]).unwrap();
    let by_name = JoinCode::create(&[], SECRET, Some(CREATED))
        .unwrap()
        .with_relays(&[relay("relay.feverlink.example:28100")])
        .unwrap();
    // version, created, secret, endpoints, extension count, tag, length, count, family, address, port, check
    assert_eq!(base32_decode(&by_ip.encode()).unwrap().len(), 1 + 4 + 16 + 1 + 1 + 1 + 2 + 1 + 1 + 4 + 2 + 4);
    assert!(by_name.encode().len() < 100, "{}", by_name.encode().len());
}

#[test]
fn more_than_four_relays_or_eight_endpoints_cannot_be_made() {
    let five: Vec<RelayAddr> = (1..=5).map(|i| relay(&format!("203.0.113.{i}:28100"))).collect();
    let code = JoinCode::create(&[], SECRET, Some(CREATED)).unwrap();
    assert_eq!(code.clone().with_relays(&five).unwrap_err(), JoinCodeError::TooManyRelays(5));
    assert!(code.with_relays(&five[..4]).is_ok());
    assert!(JoinCodeError::TooManyRelays(5).to_string().contains("at most 4"));
}

#[test]
fn a_field_this_build_does_not_know_is_skipped_not_fatal() {
    // an unknown tag next to the relays: a later build's field (any tag but 1) must not break this decoder
    let mut body = head(2);
    body.push(2); // two extensions
    body.extend(extension(9, &[1, 2, 3, 4, 5, 6, 7, 8]));
    body.extend(extension(1, &relay_value(&[&V4_RELAY])));
    let code = JoinCode::try_decode(&seal(body)).expect("decodes");
    assert_eq!(code.relays, vec![relay("203.0.113.9:28388")]);
    assert_eq!(code.version, 2);

    // only an unknown field: a version 2 code without relays
    let mut body = head(2);
    body.push(1);
    body.extend(extension(200, &[]));
    let code = JoinCode::try_decode(&seal(body)).expect("decodes");
    assert!(code.relays.is_empty());
    assert_eq!(code.secret, SECRET);
}

#[test]
fn malformed_version_2_codes_decode_to_none() {
    let relays_ok = extension(1, &relay_value(&[&V4_RELAY]));
    let with = |count: u8, rest: Vec<u8>| {
        let mut b = head(2);
        b.push(count);
        b.extend(rest);
        seal(b)
    };
    // the shape the tests start from is good
    assert!(JoinCode::try_decode(&with(1, relays_ok.clone())).is_some());

    let mut cases: Vec<(&str, String)> = Vec::new();
    cases.push(("unknown version", {
        let mut b = head(3);
        b.push(1);
        b.extend(relays_ok.clone());
        seal(b)
    }));
    cases.push(("version 2 without an extension count", seal(head(2))));
    cases.push(("zero extensions", with(0, vec![])));
    cases.push(("nine extensions", with(9, (0..9).flat_map(|_| extension(7, &[])).collect())));
    cases.push(("count larger than what follows", with(2, relays_ok.clone())));
    cases.push(("trailing bytes after the extensions", with(1, [relays_ok.clone(), vec![0]].concat())));
    cases.push(("the relay tag twice", with(2, [relays_ok.clone(), relays_ok.clone()].concat())));
    cases.push(("length longer than the code", with(1, vec![1, 0, 200, 1, 4, 1, 2]))); // claims 200 bytes
    cases.push(("an empty relay list", with(1, extension(1, &[0]))));
    cases.push(("an empty relay value", with(1, extension(1, &[]))));
    cases.push(("five relays", with(1, extension(1, &relay_value(&[&V4_RELAY[..]; 5])))));
    cases.push(("the list claims two relays and holds one", with(1, extension(1, &[2, 4, 1, 2, 3, 4, 0, 80]))));
    cases
        .push(("bytes left in the relay value", with(1, extension(1, &[relay_value(&[&V4_RELAY]), vec![0]].concat()))));
    cases.push(("a relay with port 0", with(1, extension(1, &relay_value(&[&[4, 1, 2, 3, 4, 0, 0]])))));
    cases.push(("an unknown address family", with(1, extension(1, &relay_value(&[&[5, 1, 2, 3, 4, 0, 80]])))));
    cases.push(("an IPv4 relay cut short", with(1, extension(1, &relay_value(&[&[4, 1, 2, 3]])))));
    cases.push((
        "a name longer than the bytes left",
        with(1, extension(1, &relay_value(&[&[1, 20, b'a', b'b', 0, 80]]))),
    ));
    cases.push(("an empty name", with(1, extension(1, &relay_value(&[&[1, 0, 0, 80]])))));
    cases.push(("a name with a space", with(1, extension(1, &relay_value(&[&[1, 3, b'a', b' ', b'b', 0, 80]])))));
    cases.push(("a name with a slash", with(1, extension(1, &relay_value(&[&[1, 3, b'a', b'/', b'b', 0, 80]])))));
    cases.push(("a name that is not UTF-8", with(1, extension(1, &relay_value(&[&[1, 2, 0xFF, 0xFE, 0, 80]])))));
    cases.push((
        "a numeric top label",
        with(1, extension(1, &relay_value(&[&[1, 7, b'1', b'.', b'2', b'.', b'3', b'.', b'4', 0, 80]]))),
    ));
    for (what, text) in cases {
        assert_eq!(JoinCode::try_decode(&text), None, "{what}");
    }
}

#[test]
fn every_truncation_of_a_valid_version_2_code_is_refused_even_with_a_correct_check() {
    let code = JoinCode::create(&[lan()], SECRET, Some(CREATED))
        .unwrap()
        .with_relays(&[relay("relay.example.org:28100"), relay("203.0.113.9:28100")])
        .unwrap();
    let bytes = base32_decode(&code.encode()).unwrap();
    let body = &bytes[..bytes.len() - 4];
    for n in 0..body.len() {
        assert_eq!(JoinCode::try_decode(&seal(body[..n].to_vec())), None, "cut to {n} of {} bytes", body.len());
    }
    assert!(JoinCode::try_decode(&seal(body.to_vec())).is_some());
}

#[test]
fn a_changed_character_in_a_version_2_code_is_refused() {
    let text = JoinCode::create(&[lan()], SECRET, Some(CREATED))
        .unwrap()
        .with_relays(&[relay("relay.example.org:28100")])
        .unwrap()
        .encode();
    for i in 0..text.len() {
        let c = if &text[i..=i] == "A" { "B" } else { "A" };
        let broken = format!("{}{}{}", &text[..i], c, &text[i + 1..]);
        assert_eq!(None, JoinCode::try_decode(&broken), "position {i}");
    }
}

#[test]
fn relay_addresses_parse_in_every_shape_people_type() {
    assert_eq!(relay("relay.example.org:28100").to_string(), "relay.example.org:28100");
    assert_eq!(relay("203.0.113.9:80").socket_addr(), Some("203.0.113.9:80".parse().unwrap()));
    assert_eq!(relay("[2001:db8::7]:28100").to_string(), "[2001:db8::7]:28100");
    assert_eq!(relay("[2001:db8::7]:28100").host(), "2001:db8::7");
    assert_eq!(relay("  Relay.Example.ORG:1  ").to_string(), "relay.example.org:1", "case and spaces do not matter");
    assert_eq!(relay("relay.example.org.:1").to_string(), "relay.example.org:1", "a final dot is dropped");
    assert_eq!(relay("localhost:28100").socket_addr(), None, "a name is not an address until someone resolves it");
    assert_eq!(RelayAddr::parse("relay.example.org", Some(28100)).unwrap().port(), 28100);
    assert_eq!(RelayAddr::parse("[::1]", Some(28100)).unwrap().to_string(), "[::1]:28100");
    assert_eq!(RelayAddr::from("127.0.0.1:5".parse::<SocketAddr>().unwrap()).to_string(), "127.0.0.1:5");
    for bad in [
        "",
        " ",
        ":80",
        "relay.example.org",
        "relay.example.org:0",
        "relay.example.org:65536",
        "relay.example.org:x",
        "a:b:80",
        "::1",
        "[::1",
        "[::1]x",
        "[::1]:",
        "-bad.example.org:80",
        "bad-.example.org:80",
        "ba d.example.org:80",
        "under_score.example.org:80",
        "999.1.1.1:80",
        "1.2.3:80",
        "..:80",
        "a..b:80",
    ] {
        assert_eq!(RelayAddr::parse(bad, None), None, "{bad:?}");
    }
    assert_eq!(RelayAddr::parse(&format!("{}:80", "a".repeat(101)), None), None, "longer than 100");
    assert_eq!(RelayAddr::parse(&format!("{}.org:80", "a".repeat(64)), None), None, "a label longer than 63");
    assert!(
        RelayAddr::parse(&format!("{}:80", ["a".repeat(30), "b".repeat(30), "org".into()].join(".")), None).is_some()
    );
}

#[test]
fn relay_addresses_display_what_they_parse() {
    for text in ["relay.example.org:28100", "203.0.113.9:28100", "[2001:db8::7]:28100", "[::1]:1", "a:65535"] {
        assert_eq!(relay(text).to_string(), text);
        assert_eq!(relay(&relay(text).to_string()), relay(text));
    }
}
