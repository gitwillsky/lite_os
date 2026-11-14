//! 会话只采样当前页。计数仍由转发服务独占，网关只保留上次发送的采样。

use super::*;
use router_protocol::firewall::counters::{Cursor, Page, observation};

pub(super) fn serve(session: &mut Session, control: &[u8]) -> io::Result<()> {
    let (domain, query) =
        administration::parse_scoped_query(control).map_err(|_| io::ErrorKind::InvalidData)?;
    let cursor = Cursor::parse(query).map_err(|_| io::ErrorKind::InvalidData)?;
    let result = observe(session, cursor, |cursor| sample(domain, cursor));
    super::observation::report_failure(session, result, "无法继续读取防火墙计数")
}

fn observe(
    session: &mut Session,
    mut cursor: Cursor,
    mut sample: impl FnMut(Cursor) -> io::Result<Page>,
) -> io::Result<()> {
    let mut previous = None;
    super::observation::stream(session, || {
        let page = match sample(cursor) {
            Ok(page) => page,
            Err(error) if cursor.offset != 0 => {
                // 只有首页确证换版才重置页码，保留服务故障或相同版本游标错误。
                match sample(Cursor::default()) {
                    Ok(page) if page.revision != cursor.revision => {
                        cursor = Cursor::default();
                        previous = None;
                        page
                    }
                    _ => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        let update = observation::encode(cursor, &page, previous.as_ref())
            .map_err(|_| io::ErrorKind::InvalidData)?;
        previous = Some(page);
        Ok(update)
    })
}

fn sample(domain: Option<&str>, cursor: Cursor) -> io::Result<Page> {
    let bytes = read_management_page(ordinal::FIREWALL_COUNTERS, domain, &cursor.encode())?;
    Page::parse(&bytes, cursor).map_err(|_| io::ErrorKind::InvalidData.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use router_protocol::firewall::{
        Destination, Ingress, PreroutingAction, Rule, counters::Counter,
    };
    use tungstenite::protocol::Role;

    #[test]
    fn stale_page_resets_and_the_idle_socket_still_delivers_counter_updates() {
        let (client, server) = UnixStream::pair().unwrap();
        thread::scope(|scope| {
            let requested = Cursor {
                revision: 7,
                offset: 64,
            };
            let worker = scope.spawn(move || {
                let mut session = WebSocket::from_raw_socket(
                    SessionIo::Unix(DeadlineStream::new(server)),
                    Role::Server,
                    Some(wire_config()),
                );
                let mut packets = 0;
                observe(&mut session, requested, |cursor| {
                    if cursor == requested {
                        return Err(io::Error::other("规则链已换版"));
                    }
                    assert_eq!(cursor, Cursor::default());
                    packets += 1;
                    Ok(Page {
                        revision: 8,
                        next: None,
                        rows: vec![Counter {
                            rule: Rule {
                                ingress: Ingress::Any,
                                source: None,
                                destination: Destination::Any,
                                action: PreroutingAction::Accept,
                                connection_state: None,
                                transport: None,
                            },
                            chain: None,
                            position: 1,
                            jump_chain: None,
                            destination_set: None,
                            packets,
                            bytes: packets * 64,
                        }],
                    })
                })
            });
            let mut session = WebSocket::from_raw_socket(
                SessionIo::Unix(DeadlineStream::new(client)),
                Role::Client,
                Some(wire_config()),
            );
            let next = |session: &mut Session| {
                let message = receive(session).unwrap();
                let (body, _) =
                    success_control(management::WATCH_FIREWALL_COUNTERS, &message).unwrap();
                observation::Update::parse(body).unwrap()
            };
            assert!(matches!(
                next(&mut session),
                observation::Update::Snapshot { cursor, page }
                    if cursor == Cursor::default() && page.revision == 8 && page.rows[0].packets == 1
            ));
            assert!(matches!(
                next(&mut session),
                observation::Update::Counts { revision: 8, values } if values[0].packets == 2
            ));
            drop(session);
            worker.join().unwrap().unwrap();
        });
    }
}
