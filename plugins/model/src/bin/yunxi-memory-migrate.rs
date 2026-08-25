fn main() {
    let runtime = kovi::tokio::runtime::Runtime::new().expect("create migration runtime");
    match runtime.block_on(model::run_memory_v2_migration_cli(
        std::env::args().skip(1).collect(),
    )) {
        Ok(report) => println!("{report}"),
        Err(error) => {
            eprintln!("memory v2 migration failed: {error:#}");
            std::process::exit(1);
        }
    }
}
