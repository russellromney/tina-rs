fn main() -> anyhow::Result<()> {
    let config = system_bounded_object_lane::RunConfig::from_env();
    #[cfg(feature = "real-s3")]
    let report = if std::env::var("OBJECT_LANE_S3_BUCKET").is_ok() {
        let s3 = system_bounded_object_lane::RealS3Config::from_env()?;
        system_bounded_object_lane::run_real_s3(config, s3)?
    } else {
        system_bounded_object_lane::run(config)?
    };
    #[cfg(not(feature = "real-s3"))]
    let report = system_bounded_object_lane::run(config)?;
    println!("{report:#?}");
    Ok(())
}
