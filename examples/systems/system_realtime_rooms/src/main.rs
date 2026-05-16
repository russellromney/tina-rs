fn main() {
    let report = system_realtime_rooms::run(Default::default()).expect("run");
    println!("{report:#?}");
}
