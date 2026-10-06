//! The default gateway from the Linux routing table's text.
//!
//! Read from `/proc/net/route`: a header line, then a route a line, its
//! fields in columns. A default route goes to 0.0.0.0/0, is up and goes
//! through a gateway; of several, the lowest metric is the one the kernel
//! takes. An address is its network-order word printed as the machine's own
//! integer, so the parsed integer's own bytes are the octets.

use core::net::Ipv4Addr;

const RTF_UP: u32 = 0x0001;
const RTF_GATEWAY: u32 = 0x0002;

/// The gateway of the default route with the lowest metric.
pub fn default_gateway(table: &str) -> Option<Ipv4Addr> {
    table
        .lines()
        .skip(1)
        .filter_map(default_route)
        .min_by_key(|&(_, metric)| metric)
        .map(|(gateway, _)| gateway)
}

/// A default route's gateway and metric; nothing for any other line.
fn default_route(line: &str) -> Option<(Ipv4Addr, u32)> {
    let mut fields = line.split_whitespace();
    let _interface = fields.next()?;
    let destination = hex(fields.next()?)?;
    let gateway = hex(fields.next()?)?;
    let flags = hex(fields.next()?)?;
    let _references = fields.next()?;
    let _uses = fields.next()?;
    let metric = metric(fields.next()?)?;
    let mask = hex(fields.next()?)?;
    let through_gateway = flags & (RTF_UP | RTF_GATEWAY) == RTF_UP | RTF_GATEWAY;
    (destination == 0 && mask == 0 && through_gateway && gateway != 0)
        .then(|| (Ipv4Addr::from(gateway.to_ne_bytes()), metric))
}

/// A metric as the kernel prints it: an unsigned priority written as a signed
/// integer, so one past the sign bit reads negative and is the same word.
fn metric(field: &str) -> Option<u32> {
    field.parse::<u32>().ok().or_else(|| {
        field
            .parse::<i32>()
            .ok()
            .map(|signed| u32::from_ne_bytes(signed.to_ne_bytes()))
    })
}

fn hex(field: &str) -> Option<u32> {
    if field.is_empty() || !field.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(field, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str =
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n";

    // The table as a little-endian machine prints it; the parse reads the
    // machine's own byte order, so this is the order the tests run in.
    #[cfg(target_endian = "little")]
    #[test]
    fn the_default_route_with_the_lowest_metric() {
        let table = format!(
            "{HEADER}\
             wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
             eth0\t00000000\t010AA8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
             eth0\t000AA8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
             docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0\n"
        );
        assert_eq!(
            default_gateway(&table),
            Some(Ipv4Addr::new(192, 168, 10, 1))
        );
    }

    /// A metric past the sign bit is printed negative and is still a route:
    /// alone it is the default, beside an ordinary one it is the higher.
    #[cfg(target_endian = "little")]
    #[test]
    fn a_metric_printed_negative_is_a_route_with_a_high_metric() {
        let high = "tun0\t00000000\t0101A8C0\t0003\t0\t0\t-1\t00000000\t0\t0\t0\n";
        let ordinary = "eth0\t00000000\t010AA8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n";
        assert_eq!(
            default_gateway(&format!("{HEADER}{high}")),
            Some(Ipv4Addr::new(192, 168, 1, 1))
        );
        assert_eq!(
            default_gateway(&format!("{HEADER}{high}{ordinary}")),
            Some(Ipv4Addr::new(192, 168, 10, 1))
        );
    }

    #[cfg(target_endian = "little")]
    #[test]
    fn routes_that_are_not_a_default_through_a_gateway() {
        for line in [
            // Down.
            "eth0\t00000000\t0101A8C0\t0002\t0\t0\t0\t00000000\t0\t0\t0",
            // On the link, no gateway.
            "eth0\t00000000\t00000000\t0001\t0\t0\t0\t00000000\t0\t0\t0",
            // A subnet, not the default.
            "eth0\t0000A8C0\t0101A8C0\t0003\t0\t0\t0\t0000FFFF\t0\t0\t0",
            // Fields missing, or not numbers.
            "eth0\t00000000\t0101A8C0\t0003\t0\t0",
            "eth0\t00000000\tzz01A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0",
            "eth0\t00000000\t0101A8C0\t0003\t0\t0\tx\t00000000\t0\t0\t0",
            "eth0\t00000000\t+101A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0",
        ] {
            assert_eq!(
                default_gateway(&format!("{HEADER}{line}\n")),
                None,
                "{line}"
            );
        }
        assert_eq!(default_gateway(""), None);
        // The header is never a route, whatever it holds.
        let headless = "eth0\t00000000\t0101A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0\n";
        assert_eq!(default_gateway(headless), None);
    }
}
