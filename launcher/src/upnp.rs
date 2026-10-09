//! Making a host reachable from the internet without the player touching the router: UPnP port mapping (the
//! router's Internet Gateway Device service, found by SSDP and driven over SOAP) and, when that is not on offer, a
//! plain public-IP lookup so the join code at least names the right address.
//!
//! Everything that parses text is a pure function with tests; the functions that talk to the network are thin and
//! return a readable reason on failure, because a router that refuses is the normal case, not an error.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

const SSDP_ADDR: &str = "239.255.255.250:1900";
const SERVICES: [&str; 2] =
    ["urn:schemas-upnp-org:service:WANIPConnection:1", "urn:schemas-upnp-org:service:WANPPPConnection:1"];
const IO_TIMEOUT: Duration = Duration::from_secs(4);
/// A whole HTTP exchange takes no longer than this, however slowly the other side drips its answer.
const HTTP_DEADLINE: Duration = Duration::from_secs(10);
/// The most of an answer that is read (a device description is a few kilobytes).
const MAX_RESPONSE: usize = 256 * 1024;
/// A mapping lives two hours unless it is renewed or removed.
const LEASE_SECONDS: u32 = 7200;

/// What mapping a port gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub external_ip: Option<Ipv4Addr>,
    pub control_url: String,
    pub service: String,
}

/// The `LOCATION:` header of an SSDP response, if it points at a device on a private network.
pub fn parse_location(response: &str) -> Option<String> {
    response
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case("location").then(|| v.trim().to_owned())
        })
        .filter(|l| is_lan_url(l))
}

/// Whether `url` is `http://` to an IPv4 address on a private network (or link-local, or shared address space),
/// and nothing else: no name, no user, no loopback, no address on the internet. Where a router is. Any device on the
/// LAN can answer the discovery request and choose the URLs that are fetched and posted to afterwards; this keeps
/// it from sending the game process to a host of its own choosing.
pub fn is_lan_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    };
    host.parse::<Ipv4Addr>().is_ok_and(|ip| is_private(ip) && !ip.is_loopback())
}

/// The control URL of the first WAN connection service in a device description, and the service type it is for.
/// `base` is the description's own URL (`http://host:port/path`), used for relative control URLs.
pub fn parse_control_url(xml: &str, base: &str) -> Option<(String, String)> {
    for service in SERVICES {
        let Some(at) = xml.find(service) else { continue };
        let rest = &xml[at..];
        let end = rest.find("</service>").unwrap_or(rest.len());
        let block = &rest[..end];
        let url = tag(block, "controlURL")?;
        return Some((absolute(&url, base)?, service.to_owned()));
    }
    None
}

fn tag(text: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let a = text.find(&open)? + open.len();
    let b = text[a..].find(&close)? + a;
    Some(text[a..b].trim().to_owned())
}

fn absolute(url: &str, base: &str) -> Option<String> {
    let full = if url.starts_with("http://") {
        url.to_owned()
    } else {
        let rest = base.strip_prefix("http://")?;
        let authority = rest.split('/').next()?;
        format!("http://{authority}/{}", url.trim_start_matches('/'))
    };
    is_lan_url(&full).then_some(full)
}

/// `(host:port, path)` of an `http://` URL. `None` for anything with a space or a control character in it: the
/// parts go into a request line and a header, where a line break would start a new header.
pub fn split_url(url: &str) -> Option<(String, String)> {
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let authority = if authority.contains(':') { authority.to_owned() } else { format!("{authority}:80") };
    Some((authority, path.to_owned()))
}

fn soap_body(service: &str, action: &str, args: &[(&str, String)]) -> String {
    let mut inner = String::new();
    for (k, v) in args {
        inner.push_str(&format!("<{k}>{v}</{k}>"));
    }
    format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{service}\">{inner}</u:{action}></s:Body></s:Envelope>"
    )
}

/// The external address in a `GetExternalIPAddress` response.
pub fn parse_external_ip(xml: &str) -> Option<Ipv4Addr> {
    tag(xml, "NewExternalIPAddress")?.parse().ok()
}

/// True for an address a router would not hand to the internet (private, shared, loopback, link-local): a
/// "public" address that is one of these means a double NAT, where mapping does not help.
pub fn is_private(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_private() || ip.is_loopback() || ip.is_link_local() || (o[0] == 100 && (64..128).contains(&o[1]))
}

fn http(method: &str, url: &str, headers: &[(&str, String)], body: &str) -> Result<(u16, String), String> {
    let (authority, path) = split_url(url).ok_or("bad url")?;
    let addr: SocketAddr = authority.to_socket_addrs().map_err(|e| e.to_string())?.next().ok_or("no address")?;
    let mut stream = TcpStream::connect_timeout(&addr, IO_TIMEOUT).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
    for (k, v) in headers {
        request.push_str(&format!("{k}: {v}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    // read until the end, the size limit or the deadline: a device that sends a byte every few seconds would
    // otherwise keep the lookup thread for ever (the timeout above is per read)
    let deadline = Instant::now() + HTTP_DEADLINE;
    let (mut raw, mut chunk) = (Vec::new(), [0u8; 8192]);
    while raw.len() < MAX_RESPONSE {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        stream.set_read_timeout(Some(left.min(IO_TIMEOUT))).ok();
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => raw.extend_from_slice(chunk.get(..n).unwrap_or_default()),
        }
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or("no HTTP status")?;
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_owned();
    Ok((status, body))
}

fn soap(control_url: &str, service: &str, action: &str, args: &[(&str, String)]) -> Result<String, String> {
    let body = soap_body(service, action, args);
    let (status, response) = http(
        "POST",
        control_url,
        &[("Content-Type", "text/xml; charset=\"utf-8\"".into()), ("SOAPAction", format!("\"{service}#{action}\""))],
        &body,
    )?;
    if status == 200 {
        Ok(response)
    } else {
        let why = tag(&response, "errorDescription").unwrap_or_else(|| format!("HTTP {status}"));
        Err(why)
    }
}

/// Finds the router by SSDP and returns the URL of its description.
fn discover(local: Ipv4Addr) -> Result<String, String> {
    let socket = UdpSocket::bind(SocketAddr::from((local, 0))).map_err(|e| e.to_string())?;
    socket.set_read_timeout(Some(Duration::from_millis(500))).ok();
    for st in ["urn:schemas-upnp-org:device:InternetGatewayDevice:1", "upnp:rootdevice"] {
        let search =
            format!("M-SEARCH * HTTP/1.1\r\nHOST: {SSDP_ADDR}\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {st}\r\n\r\n");
        let _ = socket.send_to(search.as_bytes(), SSDP_ADDR);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        if let Ok((n, _)) = socket.recv_from(&mut buf) {
            if let Some(location) = parse_location(&String::from_utf8_lossy(&buf[..n])) {
                // a rootdevice that is not a router answers too: only take one whose description has a WAN service
                if let Ok((200, xml)) = http("GET", &location, &[], "") {
                    if parse_control_url(&xml, &location).is_some() {
                        return Ok(location);
                    }
                }
            }
        }
    }
    Err("no UPnP router answered".into())
}

/// Asks the router to forward TCP `port` to this machine (`local`). On success the router's own report of its
/// external address is included when it gives one.
pub fn map_port(local: Ipv4Addr, port: u16, description: &str) -> Result<Mapping, String> {
    let location = discover(local)?;
    let (_, xml) = http("GET", &location, &[], "")?;
    let (control_url, service) =
        parse_control_url(&xml, &location).ok_or("the router has no WAN connection service")?;
    soap(
        &control_url,
        &service,
        "AddPortMapping",
        &[
            ("NewRemoteHost", String::new()),
            ("NewExternalPort", port.to_string()),
            ("NewProtocol", "TCP".into()),
            ("NewInternalPort", port.to_string()),
            ("NewInternalClient", local.to_string()),
            ("NewEnabled", "1".into()),
            ("NewPortMappingDescription", description.to_owned()),
            ("NewLeaseDuration", LEASE_SECONDS.to_string()),
        ],
    )
    .map_err(|e| format!("the router refused the port mapping ({e})"))?;
    let external_ip =
        soap(&control_url, &service, "GetExternalIPAddress", &[]).ok().and_then(|r| parse_external_ip(&r));
    Ok(Mapping { external_ip, control_url, service })
}

/// Removes a mapping made by [`map_port`]. Best effort.
pub fn unmap_port(mapping: &Mapping, port: u16) {
    let _ = soap(
        &mapping.control_url,
        &mapping.service,
        "DeletePortMapping",
        &[("NewRemoteHost", String::new()), ("NewExternalPort", port.to_string()), ("NewProtocol", "TCP".into())],
    );
}

/// The address the internet sees this machine as, from a plain-text service (HTTP, so no TLS library is needed;
/// the answer is only ever shown to the player and put in a join code, never trusted for security).
pub fn public_ip_lookup() -> Result<Ipv4Addr, String> {
    let mut last = String::from("no lookup service answered");
    for url in ["http://api.ipify.org/", "http://checkip.amazonaws.com/", "http://ipv4.icanhazip.com/"] {
        match http("GET", url, &[("User-Agent", "FeverLink3".into())], "") {
            Ok((200, body)) => match body.trim().parse::<Ipv4Addr>() {
                Ok(ip) => return Ok(ip),
                Err(_) => last = format!("{url} answered something else"),
            },
            Ok((status, _)) => last = format!("{url} answered HTTP {status}"),
            Err(e) => last = format!("{url}: {e}"),
        }
    }
    Err(last)
}

/// This machine's address towards the internet, what the router must forward to.
pub fn local_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESCRIPTION: &str = r#"<?xml version="1.0"?><root><device><deviceList><device><deviceList><device>
        <serviceList>
        <service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:WANIPConn1</serviceId>
        <controlURL>/ctl/IPConn</controlURL><eventSubURL>/evt/IPConn</eventSubURL></service>
        </serviceList></device></deviceList></device></deviceList></device></root>"#;

    #[test]
    fn location_header_is_found_case_insensitively() {
        let r = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nLocation: http://192.168.0.1:5000/rootDesc.xml\r\nST: upnp:rootdevice\r\n\r\n";
        assert_eq!(parse_location(r).as_deref(), Some("http://192.168.0.1:5000/rootDesc.xml"));
        assert_eq!(parse_location("HTTP/1.1 200 OK\r\nLOCATION: https://x/y\r\n"), None);
        assert_eq!(parse_location("nothing here"), None);
    }

    #[test]
    fn control_url_is_resolved_against_the_description() {
        let (url, service) = parse_control_url(DESCRIPTION, "http://192.168.0.1:5000/rootDesc.xml").unwrap();
        assert_eq!(url, "http://192.168.0.1:5000/ctl/IPConn");
        assert!(service.ends_with("WANIPConnection:1"));
        let absolute = DESCRIPTION.replace("/ctl/IPConn", "http://10.0.0.1:49152/ctl");
        assert_eq!(parse_control_url(&absolute, "http://192.168.0.1:5000/x").unwrap().0, "http://10.0.0.1:49152/ctl");
        assert!(parse_control_url("<root/>", "http://a/").is_none());
    }

    #[test]
    fn only_urls_on_the_private_network_are_followed() {
        for good in [
            "http://192.168.0.1:5000/rootDesc.xml",
            "http://10.0.0.1/",
            "http://172.16.5.5:80",
            "http://100.64.0.1:1/x?y#z",
        ] {
            assert!(is_lan_url(good), "{good}");
        }
        for bad in [
            "http://8.8.8.8/",
            "http://203.0.113.9:80/ctl",
            "http://router.local/",
            "http://evil.example/192.168.0.1",
            "http://192.168.0.1@evil.example/",
            "http://user@192.168.0.1/",
            "http://127.0.0.1:8080/",
            "http://0.0.0.0/",
            "http://[fe80::1]/",
            "https://192.168.0.1/",
            "http://192.168.0.1.evil.example/",
            "http://192.168.0.256/",
            "ftp://192.168.0.1/",
            "192.168.0.1",
            "",
        ] {
            assert!(!is_lan_url(bad), "{bad}");
        }
        // a device on the LAN that answers discovery cannot point the lookups outside
        let hostile = "HTTP/1.1 200 OK\r\nLOCATION: http://203.0.113.9/desc.xml\r\n\r\n";
        assert_eq!(parse_location(hostile), None);
        let xml = DESCRIPTION.replace("/ctl/IPConn", "http://203.0.113.9:49152/ctl");
        assert!(parse_control_url(&xml, "http://192.168.0.1:5000/x").is_none());
        assert!(parse_control_url(DESCRIPTION, "http://203.0.113.9/x").is_none(), "nor can the base be elsewhere");
    }

    #[test]
    fn a_url_cannot_smuggle_a_header_into_the_request() {
        assert_eq!(split_url("http://192.168.0.1/a\r\nX-Evil: 1"), None);
        assert_eq!(split_url("http://192.168.0.1/a b"), None);
        assert_eq!(split_url("http://192.168.0.1/\u{0}"), None);
        assert!(split_url("http://192.168.0.1:5000/ctl/IPConn?x=1").is_some());
    }

    #[test]
    fn urls_split_into_authority_and_path() {
        assert_eq!(split_url("http://192.168.0.1:5000/a/b"), Some(("192.168.0.1:5000".into(), "/a/b".into())));
        assert_eq!(split_url("http://router/"), Some(("router:80".into(), "/".into())));
        assert_eq!(split_url("https://router/"), None);
    }

    #[test]
    fn external_ip_is_read_from_the_soap_answer() {
        let xml = "<s:Envelope><s:Body><u:GetExternalIPAddressResponse><NewExternalIPAddress>203.0.113.9</NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>";
        assert_eq!(parse_external_ip(xml), Some(Ipv4Addr::new(203, 0, 113, 9)));
        assert_eq!(parse_external_ip("<x/>"), None);
    }

    #[test]
    fn soap_requests_carry_the_arguments() {
        let body =
            soap_body("urn:x:service:WANIPConnection:1", "AddPortMapping", &[("NewExternalPort", "28090".into())]);
        assert!(body.contains("<u:AddPortMapping xmlns:u=\"urn:x:service:WANIPConnection:1\">"));
        assert!(body.contains("<NewExternalPort>28090</NewExternalPort>"));
    }

    #[test]
    fn private_and_shared_addresses_are_not_public() {
        for ip in [
            [192, 168, 1, 4],
            [10, 0, 0, 1],
            [172, 16, 5, 5],
            [100, 64, 0, 1],
            [100, 124, 107, 77],
            [127, 0, 0, 1],
            [169, 254, 1, 1],
        ] {
            assert!(is_private(Ipv4Addr::from(ip)), "{ip:?}");
        }
        assert!(!is_private(Ipv4Addr::new(203, 0, 113, 9)));
        assert!(!is_private(Ipv4Addr::new(100, 63, 0, 1)));
    }
}
