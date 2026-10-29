//! TCP纯时间生命周期和keepalive实际probe属于transport，不能借路由或getter模拟。
use super::SourceFile;
use std::path::Path;

fn normalize(text: &str) -> String {
    text.split_whitespace().collect()
}
fn check_transport(text: &str, errors: &mut Vec<String>) {
    let text = normalize(text);
    if !text.contains("pubfnmaintenance_deadline(")
        || !text.contains("pubfnmaintain(&mutself,timestamp:Instant)")
        || !text.contains("self.failure_reason=Some(FailureReason::TimedOut)")
        || !text.contains("self.keep_alive_probes>=self.keep_alive_count")
        || !text.contains("self.tx_buffer.is_empty()&&self.send_seq_no==self.local_seq_no")
        || !text.contains("self.keep_alive_at=Some(timestamp+Duration::from_millis(500))")
        || !text.contains("self.keep_alive_at=Some(timestamp+self.keep_alive_interval)")
        || !text.contains("self.keep_alive_probes=self.keep_alive_probes.saturating_add(1)")
        || !text.contains("self.keep_alive_probes=0")
        || !text.contains("constCLOSE_DELAY:Duration=Duration::from_millis(60_000)")
        || !text.contains("self.rtte.initialize_route(self.route_initial_rtt)")
        || !text.contains("self.timer.set_for_retransmit(timestamp,self.retry_delay(timestamp))")
        || !text.contains("self.retry_expired(timestamp)")
        || !text.contains("self.state!=State::SynSent||self.retransmits>4")
        || !text.contains("(Some(start),Some(timeout))=(self.pending_since,self.user_timeout)")
        || !text.contains("pubfnclose_orphan(&mutself,timestamp:Instant)")
        || !text.contains("self.orphaned&&self.state==State::FinWait2")
        || !text.contains("fnfin_wait2_deadline(&self)")
        || !text.contains("self.fin_wait2_since=Some(cx.now())")
        || !text.contains("self.linger2<0")
        || !text.contains("letack_progress=repr.ack_number.is_some_and(|ack|ack>self.local_seq_no)")
        || !text.contains("ifack_progress{self.retransmits=0;self.zero_window_since=None;")
        || !text.contains("self.zero_window_since.zip(self.user_timeout)")
        || !text.contains("self.zero_window_probes=self.zero_window_probes.saturating_add(1)")
        || !text
            .contains("self.timer.schedule_zero_window_probe(timestamp,self.probe_user_deadline())")
        || !text.contains("self.timer.set_for_zero_window_probe(None,delay)")
        || !text.contains(
            "repr.seq_number=self.local_seq_no-1;repr.payload=&[];repr.control=TcpControl::None",
        )
        || !text.contains("letack_max=self.send_seq_no")
        || !text.contains("ack_all=self.send_seq_no<=ack_number")
        || !text.contains("ifself.remote_last_seq>self.send_seq_no{self.send_seq_no=self.remote_last_seq;")
        || text.contains("letack_max=self.local_seq_no+unacknowledged")
        || !text.contains("pubfnoutput_unavailable(&mutself,timestamp:Instant,failure:OutputFailure)")
        || !text.contains("ifself.keep_alive_due(timestamp){self.finish_keep_alive(timestamp,congested);")
        || !text.contains("ifcongested{self.keep_alive_at=Some(timestamp+Duration::from_millis(500));}else{self.keep_alive_probes=")
    {
        errors.push("tcp-timer: actual pure clock/terminal cause and native idle/interval/count must progress independently of route; congestion cannot consume peer probes".into());
    }
    let emission = text.find("ifletErr(error)=emit(cx,(ip_repr,repr))");
    let completion = text.find("ifis_keep_alive{self.finish_keep_alive(cx.now(),false);");
    if !matches!((emission, completion), (Some(a), Some(b)) if a < b)
        || !text.contains("self.output_unavailable(cx.now(),error.into())")
    {
        errors.push(
            "tcp-timer: actual submission must precede successful probe completion; typed local congestion must never consume a probe"
                .into(),
        );
    }
}
pub(super) fn check(root: &Path, sources: &[SourceFile], errors: &mut Vec<String>) {
    let source = |path| {
        normalize(
            &sources
                .iter()
                .find(|file| file.relative == path)
                .expect("TCP timer source")
                .text,
        )
    };
    let poll = source("kernel/src/net/inet/poll.rs");
    let timing = source("kernel/src/net/inet/timing.rs");
    let owner = source("kernel/src/net/inet/tcp/io.rs");
    let options = source("kernel/src/net/inet/options.rs");
    let lifecycle = source("kernel/src/net/inet/tcp/lifecycle.rs");
    let replies = source("kernel/src/net/inet/route_reply.rs");
    if !matches!((poll.find("tcp::maintain_clocks(self,timestamp)"), poll.find("if!interface.state.up")), (Some(a), Some(b)) if a < b)
        || !timing.contains("tcp.maintenance_deadline()")
        || !owner.contains("socket.maintain(timestamp)")
        || !owner.contains("tcp.failure_reason()==Some(tcp::FailureReason::TimedOut)")
        || !options.contains("socket.set_keep_alive_policy(")
        || !options.contains("socket.set_user_timeout(")
        || !options.contains("socket.set_syn_retries(self.syn_retries)")
        || !options.contains("socket.set_linger2(self.linger2)")
        || !lifecycle.contains("tcp.close_orphan(super::super::now())")
        || !lifecycle.contains("network.queue_tcp_resets(super::super::now())")
        || !poll.contains("self.queue_tcp_resets(timestamp)")
        || !poll.contains("socket.output_unavailable(timestamp,smoltcp::socket::tcp::OutputFailure::PathUnavailable,)")
        || !replies.contains("socket.take_reset()")
        || !replies.contains("Ipv4Reply::tcp_reset(packet,socket.bound_interface())")
        || lifecycle.contains("retain_transport(")
        || lifecycle.contains("slot.fill(orphan_id,orphan)")
        || !options.contains(
            "InternetOption::KeepCount|InternetOption::SynRetriesif(1..=127).contains(&value)",
        )
        || !options.contains("keep_idle:7200,keep_interval:75,keep_count:9")
    {
        errors.push("tcp-timer: actual NET clock phase must precede NIC admission and deliver timeout once; native policy must configure real TCP handles".into());
    }
    match std::fs::read_to_string(root.join("vendor/smoltcp/src/socket/tcp.rs")) {
        Ok(text) => check_transport(&text, errors),
        Err(error) => errors.push(format!("TCP transport source: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_timer_owner_satisfies_contract() {
        let root = super::super::repository_root();
        let sources = super::super::load_sources(&root).unwrap();
        let mut errors = Vec::new();
        check(&root, &sources, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
    #[test]
    fn rejects_counting_failed_local_transmission() {
        let root = super::super::repository_root();
        let text = std::fs::read_to_string(root.join("vendor/smoltcp/src/socket/tcp.rs")).unwrap();
        let old = "self.keep_alive_probes.saturating_add(1)";
        assert!(text.contains(old));
        let text = text.replace(old, "self.keep_alive_probes.saturating_add(2)");
        syn::parse_file(&text).unwrap();
        let mut errors = Vec::new();
        check_transport(&text, &mut errors);
        assert!(!errors.is_empty());
    }
    #[test]
    fn rejects_omitting_route_independent_deadlines() {
        let root = super::super::repository_root();
        let mut sources = super::super::load_sources(&root).unwrap();
        let file = sources
            .iter_mut()
            .find(|file| file.relative == "kernel/src/net/inet/timing.rs")
            .unwrap();
        assert!(file.text.contains("tcp.maintenance_deadline()"));
        file.text = file.text.replace(
            "tcp.maintenance_deadline()",
            "None::<smoltcp::time::Instant>",
        );
        file.syntax = syn::parse_file(&file.text).unwrap();
        let mut errors = Vec::new();
        check(&root, &sources, &mut errors);
        assert!(!errors.is_empty());
    }

    #[test]
    fn rejects_attaching_fin_deadline_to_a_live_shutdown() {
        let root = super::super::repository_root();
        let text = std::fs::read_to_string(root.join("vendor/smoltcp/src/socket/tcp.rs")).unwrap();
        let old = "self.orphaned && self.state == State::FinWait2";
        assert!(text.contains(old));
        let text = text.replace(old, "self.state == State::FinWait2");
        syn::parse_file(&text).unwrap();
        let mut errors = Vec::new();
        check_transport(&text, &mut errors);
        assert!(!errors.is_empty());
    }

    #[test]
    fn rejects_retaining_reset_transport_until_link_delivery() {
        let root = super::super::repository_root();
        let mut sources = super::super::load_sources(&root).unwrap();
        let file = sources
            .iter_mut()
            .find(|file| file.relative == "kernel/src/net/inet/tcp/lifecycle.rs")
            .unwrap();
        let old = "network.queue_tcp_resets(super::super::now())";
        assert!(file.text.contains(old));
        file.text = file.text.replace(old, "network.plan_egress_routes()");
        syn::parse_file(&file.text).unwrap();
        let mut errors = Vec::new();
        check(&root, &sources, &mut errors);
        assert!(!errors.is_empty());
    }

    #[test]
    fn rejects_resetting_user_deadline_on_nonprogress_ack() {
        let root = super::super::repository_root();
        let text = std::fs::read_to_string(root.join("vendor/smoltcp/src/socket/tcp.rs")).unwrap();
        assert!(text.contains("if ack_progress {"));
        let text = text.replace("if ack_progress {", "if ack_all || ack_len > 0 {");
        syn::parse_file(&text).unwrap();
        let mut errors = Vec::new();
        check_transport(&text, &mut errors);
        // 同一predicate声明及真实使用都需要，不能仅保留一个没有调用的proof变量。
        assert!(!errors.is_empty());
    }

    #[test]
    fn rejects_using_queued_bytes_as_ack_upper_bound() {
        let root = super::super::repository_root();
        let text = std::fs::read_to_string(root.join("vendor/smoltcp/src/socket/tcp.rs")).unwrap();
        let old = "let ack_max = self.send_seq_no";
        assert!(text.contains(old));
        let text = text.replace(
            old,
            "let ack_max = self.local_seq_no + self.tx_buffer.len()",
        );
        syn::parse_file(&text).unwrap();
        let mut errors = Vec::new();
        check_transport(&text, &mut errors);
        assert!(!errors.is_empty());
    }
}
