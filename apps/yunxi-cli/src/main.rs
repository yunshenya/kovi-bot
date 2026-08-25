use std::env;
use std::io::{self, BufRead, Write};
use std::sync::Arc;

use yunxi_cli::{CliCoreState, CliHost, CliJournal, FakeEnvironment, FakeModel, HostResponse};
use yunxi_core::ConversationId;

fn main() -> io::Result<()> {
    let journal = env::var_os("YUNXI_CLI_JOURNAL")
        .map(CliJournal::open)
        .transpose()
        .map_err(io::Error::other)?
        .map(Arc::new);
    let core_state = env::var_os("YUNXI_CLI_STATE")
        .map(CliCoreState::open)
        .transpose()
        .map_err(io::Error::other)?
        .map(Arc::new);
    let conversation_id = core_state
        .as_deref()
        .map_or_else(ConversationId::new, CliCoreState::conversation_id);
    let mut host = CliHost::new(FakeModel, FakeEnvironment::default(), conversation_id);
    if let Some(core_state) = core_state {
        if let Some(path) = core_state.path() {
            eprintln!("Yunxi CLI state: {}", path.display());
        }
        host = host.with_core_state(core_state);
    }
    if let Some(journal) = journal {
        eprintln!("Yunxi CLI journal: {}", journal.path().display());
        host = host.with_journal(journal);
    }
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
