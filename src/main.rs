use std::time::Duration;
use kovi::build_bot;

fn main() {
    model::config::enable_auto_reload(Duration::from_secs(5));
    build_bot!(model).run();
}
