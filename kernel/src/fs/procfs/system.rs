use alloc::{format, string::String};
use core::fmt::Write;

use super::{ProcNetworkSnapshot, ProcSnapshot};

const CLOCK_TICKS_PER_SECOND: u64 = 100;

pub(super) fn ticks(microseconds: u64) -> u64 {
    microseconds / (1_000_000 / CLOCK_TICKS_PER_SECOND)
}

pub(super) fn format_cpu_stat(snapshot: &ProcSnapshot) -> String {
    let mut output = String::new();
    let total_busy: u64 = snapshot
        .cpus
        .iter()
        .map(|cpu| cpu.busy_us.min(snapshot.uptime_us))
        .sum();
    let total_idle = snapshot
        .uptime_us
        .saturating_mul(snapshot.cpus.len() as u64)
        .saturating_sub(total_busy);
    let _ = writeln!(
        output,
        "cpu  {} 0 0 {} 0 0 0 0",
        ticks(total_busy),
        ticks(total_idle)
    );
    for cpu in &snapshot.cpus {
        let busy = cpu.busy_us.min(snapshot.uptime_us);
        let _ = writeln!(
            output,
            "cpu{} {} 0 0 {} 0 0 0 0",
            cpu.cpu,
            ticks(busy),
            ticks(snapshot.uptime_us - busy)
        );
    }
    let _ = writeln!(output, "btime {}", snapshot.boot_epoch_seconds);
    let _ = writeln!(
        output,
        "processes {}\nprocs_running {}\nprocs_blocked 0",
        snapshot.processes_created, snapshot.runnable_tasks
    );
    output
}

pub(super) fn format_meminfo(snapshot: &ProcSnapshot) -> String {
    format!(
        "MemTotal:       {} kB\nMemFree:        {} kB\nMemAvailable:   {} kB\nBuffers:        0 kB\nCached:         0 kB\nSwapCached:     0 kB\nActive:         0 kB\nInactive:       0 kB\nSwapTotal:      0 kB\nSwapFree:       0 kB\nDirty:          0 kB\nWriteback:      0 kB\nAnonPages:      0 kB\nMapped:         0 kB\nShmem:          0 kB\nSlab:           0 kB\n",
        snapshot.total_pages * 4,
        snapshot.free_pages * 4,
        snapshot.free_pages * 4
    )
}

pub(super) fn format_loadavg(snapshot: &ProcSnapshot) -> String {
    format!(
        "{}.{:02} {}.{:02} {}.{:02} {}/{} {}\n",
        snapshot.load_milli[0] / 1000,
        snapshot.load_milli[0] / 10 % 100,
        snapshot.load_milli[1] / 1000,
        snapshot.load_milli[1] / 10 % 100,
        snapshot.load_milli[2] / 1000,
        snapshot.load_milli[2] / 10 % 100,
        snapshot.runnable_tasks,
        snapshot.total_tasks,
        snapshot.last_pid
    )
}

pub(super) fn format_uptime(snapshot: &ProcSnapshot) -> String {
    let idle_us: u64 = snapshot
        .cpus
        .iter()
        .map(|cpu| {
            snapshot
                .uptime_us
                .saturating_sub(cpu.busy_us.min(snapshot.uptime_us))
        })
        .sum();
    format!(
        "{}.{:02} {}.{:02}\n",
        snapshot.uptime_us / 1_000_000,
        snapshot.uptime_us / 10_000 % 100,
        idle_us / 1_000_000,
        idle_us / 10_000 % 100
    )
}

pub(super) fn format_network_devices(network: Option<ProcNetworkSnapshot>) -> String {
    let mut output = String::from(
        "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n",
    );
    if let Some(network) = network {
        let _ = writeln!(
            output,
            "  eth0: {:8} {:7}    0    0    0     0          0         0 {:8} {:7}    0    0    0     0       0          0",
            network.received_bytes,
            network.received_packets,
            network.transmitted_bytes,
            network.transmitted_packets,
        );
    }
    output
}

pub(super) fn format_network_routes(network: Option<ProcNetworkSnapshot>) -> String {
    let mut output = String::from(
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n",
    );
    let Some(network) = network else {
        return output;
    };
    let mask = if network.prefix_length == 0 {
        0
    } else {
        u32::MAX << (32 - network.prefix_length)
    };
    if let Some(address) = network.address {
        let destination = u32::from_be_bytes(address) & mask;
        let _ = writeln!(
            output,
            "eth0\t{:08X}\t00000000\t{:04X}\t0\t0\t0\t{:08X}\t0\t0\t0",
            destination.swap_bytes(),
            if network.up { 1 } else { 0 },
            mask.swap_bytes(),
        );
    }
    if let Some(gateway) = network.gateway {
        let _ = writeln!(
            output,
            "eth0\t00000000\t{:08X}\t{:04X}\t0\t0\t0\t00000000\t0\t0\t0",
            u32::from_be_bytes(gateway).swap_bytes(),
            if network.up { 3 } else { 2 },
        );
    }
    output
}
