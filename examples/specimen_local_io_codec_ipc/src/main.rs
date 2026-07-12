use specimen_local_io_codec_ipc::{
    SpecimenReport, admin_socket, file_ingest, framed_keyspace, live_unix_smoke,
};

fn main() -> anyhow::Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "all".to_string());
    let reports: Vec<SpecimenReport> = match mode.as_str() {
        "file-ingest" => vec![
            file_ingest::smoke(),
            file_ingest::bad_input_cap_reached(),
            file_ingest::copy_smoke(),
        ],
        "admin-socket" => vec![
            admin_socket::smoke(),
            admin_socket::bad_input_line_too_long(),
        ],
        "framed-keyspace" => vec![
            framed_keyspace::smoke(),
            framed_keyspace::bad_input_frame_too_large(),
        ],
        "live-unix" => vec![live_unix_smoke::smoke()?],
        "all" => vec![
            file_ingest::smoke(),
            file_ingest::bad_input_cap_reached(),
            file_ingest::copy_smoke(),
            admin_socket::smoke(),
            admin_socket::bad_input_line_too_long(),
            framed_keyspace::smoke(),
            framed_keyspace::bad_input_frame_too_large(),
            live_unix_smoke::smoke()?,
        ],
        other => anyhow::bail!(
            "unknown mode {other:?}; expected one of file-ingest | admin-socket | framed-keyspace | live-unix | all"
        ),
    };

    for report in &reports {
        println!(
            "specimen={} ok={} bytes={} frames={} note={}",
            report.name, report.ok, report.bytes, report.frames, report.note,
        );
    }
    if reports.iter().any(|r| !r.ok) {
        anyhow::bail!("one or more specimens failed");
    }
    Ok(())
}
