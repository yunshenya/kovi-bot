use std::env;
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use yunxi_cli::{CliCoreState, CliHost, CliJournal, FakeEnvironment, FakeModel, HostResponse};
use yunxi_core::{AutonomyPolicy, ConversationId};

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
    let policy = autonomy_policy_from_env();
    let host = CliHost::new(FakeModel, FakeEnvironment::default(), conversation_id)
        .try_with_autonomy_policy(policy)
        .map_err(io::Error::other)?;
    let host = if let Some(core_state) = core_state {
        if let Some(path) = core_state.path() {
            eprintln!("Yunxi CLI state: {}", path.display());
        }
        host.with_core_state(core_state)
    } else {
        host
    };
    let host = if let Some(journal) = journal {
        eprintln!("Yunxi CLI journal: {}", journal.path().display());
        host.with_journal(journal)
    } else {
        host
    };

    let poll_interval = env_duration("YUNXI_CLI_POLL_MS", 200);
    let autonomy_enabled = env_bool("YUNXI_CLI_AUTONOMY", true);
    let eof_grace = env_duration("YUNXI_CLI_EOF_GRACE_MS", 5_000);
    let (input_tx, input_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            if input_tx.send(line).is_err() {
                break;
            }
        }
    });

    println!("Yunxi CLI (type /quit to exit)");
    print!("You: ");
    io::stdout().flush()?;

    let mut stdin_closed_at = None;
    loop {
        match input_rx.recv_timeout(poll_interval) {
            Ok(Ok(line)) => {
                if line.trim().eq_ignore_ascii_case("/quit") {
                    break;
                }
                print_response(host.process_line(&line));
                print!("You: ");
                io::stdout().flush()?;
            }
            Ok(Err(error)) => {
                eprintln!("Yunxi: input error: {error}");
                break;
            }
            Err(RecvTimeoutError::Timeout) if autonomy_enabled => {
                match host.process_autonomous_tick() {
                    Ok(Some(response)) => {
                        print_response(Ok(response));
                        print!("You: ");
                        io::stdout().flush()?;
                    }
                    Ok(None) => {}
                    Err(error) => eprintln!("Yunxi: {error}"),
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if !autonomy_enabled {
                    break;
                }
                let closed_at = *stdin_closed_at.get_or_insert_with(Instant::now);
                if closed_at.elapsed() >= eof_grace {
                    break;
                }
                // A closed pipe still gets the same autonomous cadence as an
                // interactive terminal, allowing `printf 'hello\n' | ...` to
                // exercise the continuation loop before the process exits.
                match host.process_autonomous_tick() {
                    Ok(Some(response)) => {
                        print_response(Ok(response));
                        print!("You: ");
                        io::stdout().flush()?;
                    }
                    Ok(None) => {}
                    Err(error) => eprintln!("Yunxi: {error}"),
                }
                thread::sleep(poll_interval);
            }
        }
    }
    Ok(())
}

fn print_response(result: Result<HostResponse, yunxi_cli::CliError>) {
    match result {
        Ok(HostResponse::Empty) => {}
        Ok(HostResponse::Noop) => println!("\nYunxi: (no action)"),
        Ok(HostResponse::Delivered { message, .. }) => println!("\nYunxi: {message}"),
        Ok(HostResponse::Deferred { message, reason }) => {
            println!("\nYunxi: {message} (deferred: {reason})");
        }
        Err(error) => eprintln!("\nYunxi: {error}"),
    }
}

fn autonomy_policy_from_env() -> AutonomyPolicy {
    let idle = env_duration("YUNXI_CLI_AUTONOMY_IDLE_MS", 5_000);
    let cooldown = env_duration("YUNXI_CLI_AUTONOMY_COOLDOWN_MS", 3_000);
    let idle_ms = i64::try_from(idle.as_millis()).unwrap_or(i64::MAX).max(1);
    let cooldown_ms = i64::try_from(cooldown.as_millis())
        .unwrap_or(i64::MAX)
        .max(1);
    let idle = chrono::Duration::milliseconds(idle_ms);
    let cooldown = chrono::Duration::milliseconds(cooldown_ms);
    AutonomyPolicy {
        direct_idle: idle,
        group_idle: idle,
        direct_cooldown: cooldown,
        group_cooldown: cooldown,
        ..AutonomyPolicy::default()
    }
}

fn env_duration(name: &str, default_ms: u64) -> Duration {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(Duration::from_millis(default_ms), |value| {
            Duration::from_millis(value.clamp(1, 60_000))
        })
}

fn env_bool(name: &str, default: bool) -> bool {
    env::var(name).ok().map_or(default, |value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}
