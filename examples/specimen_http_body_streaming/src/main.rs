use specimen_http_body_streaming::{Report, tina_impl, tokio_impl};

fn main() -> anyhow::Result<()> {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "both".to_string());

    match mode.as_str() {
        "tokio" => print_side("tokio", tokio_impl::run()?),
        "tina" => print_side("tina", tina_impl::run()?),
        "both" => {
            print_side("tokio", tokio_impl::run()?);
            print_side("tina", tina_impl::run()?);
        }
        other => {
            anyhow::bail!(
                "unknown mode {other:?}; expected tokio, tina, or both. \
                 usage: specimen-http-body-streaming [tokio|tina|both]"
            );
        }
    }
    Ok(())
}

fn print_side(side: &str, report: Report) {
    fn opt(value: Option<usize>) -> String {
        value
            .map(|n| n.to_string())
            .unwrap_or_else(|| "n/a".to_string())
    }
    println!(
        "comparison=specimen_http_body_streaming side={} bytes_received={} status_ok={} \
         wall_clock_ms={} exit_clean={} tokio_response_alloc_floor={} \
         tina_response_high_water={} tina_chunked_wire_bytes={} \
         tina_chunked_decoded_bytes={} tina_capacity_discovery_line={:?}",
        side,
        report.bytes_received,
        report.status_ok,
        report.wall_clock_ms,
        report.exit_clean,
        opt(report.tokio_response_alloc_floor),
        opt(report.tina_response_high_water),
        opt(report.tina_chunked_wire_bytes),
        opt(report.tina_chunked_decoded_bytes),
        report.tina_capacity_discovery_line,
    );
}
