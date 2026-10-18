//! Minimal DNS responder wire format: just enough to answer A/IN queries
//! with synthetic addresses (PLAN_PI_IN_VM §4, D3). Not a general DNS
//! implementation — multi-question messages, EDNS, and name compression in
//! the *query* are rejected (resolvers don't send those in practice).

use alloc::string::String;
use alloc::vec::Vec;

/// A parsed single-question A/IN query.
pub struct Query<'a> {
    /// Transaction ID, echoed in the response.
    pub id: u16,
    /// Lowercased query name (e.g. `registry.npmjs.org`).
    pub name: String,
    /// The raw query packet, kept so the response can echo the question
    /// section verbatim.
    pub raw: &'a [u8],
}

fn rd16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(off)?, *b.get(off + 1)?]))
}

/// Parse a DNS query packet. Returns `None` for anything we don't answer:
/// not a query, not exactly one question, not A/IN, or a compressed name.
pub fn parse_query(pkt: &[u8]) -> Option<Query<'_>> {
    if pkt.len() < 12 {
        return None;
    }
    let id = rd16(pkt, 0)?;
    let flags = rd16(pkt, 2)?;
    if flags & 0x8000 != 0 {
        return None; // not a query
    }
    if rd16(pkt, 4)? != 1 {
        return None; // exactly one question
    }
    // Walk the qname.
    let mut off = 12;
    let mut name = String::new();
    loop {
        let len = *pkt.get(off)? as usize;
        if len == 0 {
            off += 1;
            break;
        }
        if len & 0xC0 != 0 {
            return None; // compressed name in a query: refuse
        }
        off += 1;
        let label = pkt.get(off..off + len)?;
        if !name.is_empty() {
            name.push('.');
        }
        for &c in label {
            name.push((c as char).to_ascii_lowercase());
        }
        off += len;
    }
    if rd16(pkt, off)? != 1 || rd16(pkt, off + 2)? != 1 {
        return None; // A/IN only
    }
    Some(Query { id, name, raw: pkt })
}

/// Build a response packet answering `query` with one A record (`ip`,
/// `ttl` seconds). Appends to `out`.
pub fn build_response(query: &Query, ip: [u8; 4], ttl: u32, out: &mut Vec<u8>) {
    // Header + question section: echo the query through the end of the
    // first question (12-byte header + qname + qtype + qclass).
    let mut qend = 12;
    while qend < query.raw.len() && query.raw[qend] != 0 {
        qend += 1 + query.raw[qend] as usize;
    }
    qend += 5; // root label + qtype + qclass
    let qend = qend.min(query.raw.len());
    out.extend_from_slice(&query.raw[..qend]);
    // Fix the header: QR | RD (echoed) | RA, no error; ANCOUNT=1.
    out[2] = 0x81;
    out[3] = 0x80;
    out[6] = 0;
    out[7] = 1; // ANCOUNT
    out[8] = 0;
    out[9] = 0; // NSCOUNT
    out[10] = 0;
    out[11] = 0; // ARCOUNT
    // Answer: name = pointer to the question's qname (offset 12).
    out.extend_from_slice(&[0xC0, 0x0C]);
    out.extend_from_slice(&1u16.to_be_bytes()); // TYPE A
    out.extend_from_slice(&1u16.to_be_bytes()); // CLASS IN
    out.extend_from_slice(&ttl.to_be_bytes());
    out.extend_from_slice(&4u16.to_be_bytes()); // RDLENGTH
    out.extend_from_slice(&ip);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-built query for `echo.test` A/IN, id 0x1234.
    fn query_packet() -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&0x1234u16.to_be_bytes()); // id
        p.extend_from_slice(&0x0100u16.to_be_bytes()); // RD
        p.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
        p.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
        p.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
        p.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
        p.push(4);
        p.extend_from_slice(b"echo");
        p.push(4);
        p.extend_from_slice(b"test");
        p.push(0);
        p.extend_from_slice(&1u16.to_be_bytes()); // A
        p.extend_from_slice(&1u16.to_be_bytes()); // IN
        p
    }

    #[test]
    fn parse_a_query() {
        let p = query_packet();
        let q = parse_query(&p).expect("parse");
        assert_eq!(q.id, 0x1234);
        assert_eq!(q.name, "echo.test");
    }

    #[test]
    fn response_roundtrips() {
        let p = query_packet();
        let q = parse_query(&p).unwrap();
        let mut out = Vec::new();
        build_response(&q, [10, 0, 2, 16], 60, &mut out);
        assert_eq!(&out[0..2], &0x1234u16.to_be_bytes());
        assert_eq!(out[2], 0x81);
        assert_eq!(&out[6..8], &1u16.to_be_bytes()); // one answer
        // The answer ends with the synthetic address.
        assert_eq!(&out[out.len() - 4..], &[10, 0, 2, 16]);
    }
}
