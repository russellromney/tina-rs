use specimen_multi_turn_request_context::{tina_run, tokio_run, TinaConfig, TokioConfig};

#[tokio::main]
async fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("tokio") => {
            let config = TokioConfig {
                probe_delay_ms: 10,
                db_delay_ms: 10,
            };
            let report = tokio_run(config).await.unwrap();
            println!("tokio: {:?}", report.replies);
        }
        Some("tina") => {
            let config = TinaConfig {
                probe_delay_ms: 10,
                db_delay_ms: 10,
            };
            let report = tina_run(config).unwrap();
            println!("tina: {:?}", report.replies);
        }
        Some("both") => {
            let tokio_report = tokio_run(TokioConfig {
                probe_delay_ms: 10,
                db_delay_ms: 10,
            })
            .await
            .unwrap();
            let tina_report = tina_run(TinaConfig {
                probe_delay_ms: 10,
                db_delay_ms: 10,
            })
            .unwrap();
            println!("tokio: {:?}", tokio_report.replies);
            println!("tina: {:?}", tina_report.replies);
        }
        _ => {
            eprintln!("Usage: cargo run -- [tokio|tina|both]");
        }
    }
}
