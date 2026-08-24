use std::io::{self, BufRead, Write};

use yunxi_cli::{CliHost, FakeEnvironment, FakeModel, HostResponse};
use yunxi_core::ConversationId;

fn main() -> io::Result<()> {
    let host = CliHost::new(FakeModel, FakeEnvironment::default(), ConversationId::new());
    println!("Yunxi CLI (type /quit to exit)");
    print!("You: ");
    io::stdout().flush()?;

    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.trim().eq_ignore_ascii_case("/quit") {
            break;
        }
        match host.process_line(&line) {
            Ok(HostResponse::Empty) => {}
            Ok(HostResponse::Noop) => println!("Yunxi: (no action)"),
            Ok(HostResponse::Delivered { message, .. }) => println!("Yunxi: {message}"),
            Ok(HostResponse::Deferred { message, reason }) => {
                println!("Yunxi: {message} (deferred: {reason})");
            }
            Err(error) => eprintln!("Yunxi: {error}"),
        }
        print!("You: ");
        io::stdout().flush()?;
    }
    Ok(())
}
