//! Join codes that carry the host's Steam ID (extension tag 2 of version 2): the form players actually share.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use feverlink3_core::lobby::{base32_encode, JoinCode, RelayAddr};
use sha2::{Digest, Sha256};

const SECRET: [u8; 16] = [7; 16];
const CREATED: u32 = 1_791_547_200;
const STEAM_ID: u64 = 76_561_198_152_555_654;

/// A code body as the encoder would write it, with a valid check, for shapes the encoder never writes.
fn seal(mut body: Vec<u8>) -> String {
    let check = Sha256::digest(&body);
    body.extend_from_slice(&check[..4]);
    base32_encode(&body)
}

/// Version 2, created, secret, no endpoints, then the extension list.
fn extended(extensions: &[(u8, &[u8])]) -> String {
    let mut b = vec![2u8];
    b.extend_from_slice(&CREATED.to_be_bytes());
    b.extend_from_slice(&SECRET);
    b.push(0);
    b.push(extensions.len() as u8);
    for (tag, value) in extensions {
        b.push(*tag);
        b.extend_from_slice(&(value.len() as u16).to_be_bytes());
        b.extend_from_slice(value);
    }
    seal(b)
}

fn lan() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)), 28090)
}

#[test]
fn a_steam_only_code_round_trips_and_is_short() {
    let code = JoinCode::create(&[], SECRET, Some(CREATED)).unwrap().with_steam_id(STEAM_ID);
    assert_eq!(code.version, 2);
    let text = code.encode();
    // 1 version + 4 created + 16 secret + 1 endpoint count + 1 extension count + (1 tag + 2 length + 8 id) + 4 check
    assert_eq!(text.len(), (38 * 8usize).div_ceil(5), "{text}");
    let back = JoinCode::try_decode(&text).expect("decodes");
    assert_eq!(back, code);
    assert_eq!(back.steam_id, Some(STEAM_ID));
    assert!(back.endpoints.is_empty() && back.relays.is_empty());
}

#[test]
fn steam_id_endpoints_and_relays_travel_together() {
    let relay = RelayAddr::parse("relay.example.org:443", None).unwrap();
    let code = JoinCode::create(&[lan()], SECRET, Some(CREATED))
        .unwrap()
        .with_relays(&[relay])
        .unwrap()
        .with_steam_id(STEAM_ID);
    let back = JoinCode::try_decode(&code.encode()).expect("decodes");
    assert_eq!(back, code);
    assert_eq!(back.steam_id, Some(STEAM_ID));
    assert_eq!(back.relays.len(), 1);
    assert_eq!(back.endpoints, vec![lan()]);
    // typed with lower case, dashes and spaces
    let messy = code
        .encode()
        .to_lowercase()
        .chars()
        .collect::<Vec<_>>()
        .chunks(5)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-");
    assert_eq!(JoinCode::try_decode(&messy), Some(code));
}

#[test]
fn a_code_without_a_steam_id_is_unchanged() {
    let code = JoinCode::create(&[lan()], SECRET, Some(CREATED)).unwrap();
    assert_eq!(code.version, 1);
    assert_eq!(code.encode().len(), 53);
    assert_eq!(JoinCode::try_decode(&code.encode()).unwrap().steam_id, None);
    // 0 is not a Steam ID: no field, still version 1
    let zero = code.clone().with_steam_id(0);
    assert_eq!(zero.steam_id, None);
    assert_eq!(zero.version, 1);
    assert_eq!(zero.encode(), code.encode());
}

#[test]
fn a_steam_id_can_be_removed_again() {
    let code = JoinCode::create(&[lan()], SECRET, Some(CREATED)).unwrap().with_steam_id(STEAM_ID).with_steam_id(0);
    assert_eq!(code.steam_id, None);
    assert_eq!(code.version, 1);
}

#[test]
fn malformed_steam_fields_are_rejected() {
    let id = STEAM_ID.to_be_bytes();
    assert!(JoinCode::try_decode(&extended(&[(2, &id)])).is_some());
    assert!(JoinCode::try_decode(&extended(&[(2, &[0u8; 8])])).is_none(), "id 0");
    assert!(JoinCode::try_decode(&extended(&[(2, &id[..7])])).is_none(), "too short");
    assert!(JoinCode::try_decode(&extended(&[(2, &[id.as_slice(), &[1u8]].concat())])).is_none(), "too long");
    assert!(JoinCode::try_decode(&extended(&[(2, &id), (2, &id)])).is_none(), "twice");
}

#[test]
fn unknown_tags_next_to_the_steam_id_are_skipped() {
    let id = STEAM_ID.to_be_bytes();
    let text = extended(&[(2, &id), (99, b"from a later build")]);
    assert_eq!(JoinCode::try_decode(&text).and_then(|c| c.steam_id), Some(STEAM_ID));
}
