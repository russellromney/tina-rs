fn main() {
    let profile = option_env!("PROFILE").unwrap_or("unknown");
    println!("tina portable runtime cost smoke / local machine / not benchmark");
    println!(
        "backend=betelgeuse-shaped-local profile={profile} platform={}",
        std::env::consts::OS
    );
    println!("capacities=default preallocation=default");
    println!("operation,allocations,timing,status");

    for row in [
        "local send",
        "live ingress",
        "cross-shard send",
        "isolate call",
        "timer",
        "TCP loopback",
        "TLS loopback",
        "file read/write",
        "journal append",
        "bridge call",
    ] {
        println!("{row},n/a,n/a,smoke-row");
    }
}
