use super::*;
use alloc::vec::Vec;

use packet_wire::dns::{FLAG_RA, TYPE_SOA};

const NAME: &[u8] = b"\x03www\x07example\0";
const CASE_VARIANT: &[u8] = b"\x03WWW\x07Example\0";

fn packet(
    id: u16,
    name: &[u8],
    rcode: Option<u16>,
    answers: u16,
    authorities: u16,
    edns: bool,
) -> Vec<u8> {
    let mut bytes = alloc::vec![0; HEADER_BYTES];
    Header {
        id,
        flags: FLAG_RD | rcode.map_or(0, |code| FLAG_QR | FLAG_RA | code),
        questions: 1,
        answers,
        authorities,
        additional: u16::from(edns),
    }
    .emit(&mut bytes)
    .unwrap();
    bytes.extend_from_slice(name);
    bytes.extend_from_slice(&[0, 1, 0, 1]);
    bytes
}

fn record(bytes: &mut Vec<u8>, owner: &[u8], kind: u16, class: u16, ttl: u32, data: &[u8]) {
    bytes.extend_from_slice(owner);
    bytes.extend_from_slice(&kind.to_be_bytes());
    bytes.extend_from_slice(&class.to_be_bytes());
    bytes.extend_from_slice(&ttl.to_be_bytes());
    bytes.extend_from_slice(&(data.len() as u16).to_be_bytes());
    bytes.extend_from_slice(data);
}

#[test]
fn matched_cache_hit_preserves_client_question_and_expires_negative_soa() {
    let client = packet(27, NAME, None, 0, 0, false);
    let query = Query::parse(&client).unwrap();
    let mut positive = packet(89, NAME, Some(0), 1, 0, false);
    record(&mut positive, &[0xc0, 12], 1, CLASS_IN, 60, &[192, 0, 2, 3]);
    assert!(query.verify_reply(88, &positive).is_err());

    let mut cache = Cache::new(1, 4096);
    cache
        .store(query.verify_reply(89, &positive).unwrap(), 1_000)
        .unwrap();
    let retry = packet(28, CASE_VARIANT, None, 0, 0, false);
    let retry = Query::parse(&retry).unwrap();
    let mut output = [0; 1232];
    let length = cache
        .answer(retry, 11_000, Some(1232), &mut output)
        .unwrap();
    let reply = Message::parse(&output[..length]).unwrap();
    assert_eq!(reply.header.id, 28);
    assert_eq!(reply.question.name.bytes(), CASE_VARIANT);
    assert_eq!(reply.records().next().unwrap().unwrap().ttl, 50);

    let mut negative = packet(91, NAME, Some(3), 0, 1, false);
    let mut soa = alloc::vec![0xc0, 12, 0xc0, 12];
    soa.extend_from_slice(&[0; 16]);
    soa.extend_from_slice(&5u32.to_be_bytes());
    record(&mut negative, &[0xc0, 16], TYPE_SOA, CLASS_IN, 60, &soa);
    cache
        .store(query.verify_reply(91, &negative).unwrap(), 11_000)
        .unwrap();
    let length = cache
        .answer(retry, 13_000, Some(1232), &mut output)
        .unwrap();
    let reply = Message::parse(&output[..length]).unwrap();
    assert_eq!(reply.header.rcode(), 3);
    assert_eq!(reply.records().next().unwrap().unwrap().ttl, 3);
    assert!(cache.answer(retry, 16_000, None, &mut output).is_none());
}

#[test]
fn downstream_udp_truncation_preserves_edns_and_rejects_bad_compression() {
    let mut client = packet(27, NAME, None, 0, 0, true);
    record(&mut client, &[0], TYPE_OPT, 512, DNSSEC_OK, &[]);
    let query = Query::parse(&client).unwrap();
    let mut response = packet(89, NAME, Some(0), 2, 0, true);
    let first_record = response.len();
    record(&mut response, &[0xc0, 12], 1, CLASS_IN, 60, &[192, 0, 2, 3]);
    // Unknown RDATA remains opaque and the compressed owner remains at its original offset.
    record(&mut response, &[0xc0, 12], 65500, CLASS_IN, 60, &[0; 560]);
    record(&mut response, &[0], TYPE_OPT, 1232, DNSSEC_OK, &[]);
    let answer = query.verify_reply(89, &response).unwrap();
    let mut output = [0; 1232];
    let length = answer.emit(Some(1232), &mut output).unwrap();
    let reply = Message::parse(&output[..length]).unwrap();
    assert_eq!(reply.header.id, 27);
    assert_ne!(reply.header.flags & FLAG_TC, 0);
    let opt = reply.records().next().unwrap().unwrap();
    assert_eq!((opt.kind, opt.class, opt.ttl), (TYPE_OPT, 1232, DNSSEC_OK));

    let mut cache = Cache::new(4, 4096);
    cache.store(answer, 0).unwrap();
    assert_eq!(
        cache.answer(query, 1000, Some(1232), &mut output),
        Some(length)
    );
    let tcp = cache.answer(query, 1000, None, &mut output).unwrap();
    assert_eq!(tcp, response.len());
    assert_eq!(
        Message::parse(&output[..tcp])
            .unwrap()
            .records()
            .next()
            .unwrap()
            .unwrap()
            .ttl,
        59
    );

    response[first_record + 1] = first_record as u8;
    assert!(query.verify_reply(89, &response).is_err());
}
