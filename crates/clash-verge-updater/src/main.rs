//! The privileged update component. Runs as a service (`service`), as the per-transaction worker
//! (`--worker <id>`), or as an administrative command.

mod cli;
mod daemon;
mod journal;
mod lock;
mod logger;
mod platform;
mod txn;
mod worker;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("service") => platform::run_service(),
        Some("--worker") => args
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("--worker needs a transaction id"))
            .and_then(|id| worker::run(id)),
        Some("install") => platform::install_self(),
        // Run by installers from their own copy before they write into the component's directory.
        Some("prepare") => platform::prepare_layout(&clash_verge_updater::paths::Paths::system()),
        Some("uninstall") => platform::uninstall_self(),
        Some("log") => cli::log(&args[1..]),
        Some("repair") => cli::repair(&args[1..]),
        Some("recover") => cli::recover(),
        Some("abandon") => cli::abandon(&args[1..]),
        Some("record-install") => cli::record_install(&args[1..]),
        _ => {
            eprintln!(
                "usage: clash-verge-updater <service | install | prepare | uninstall | log | repair [txn] | recover | abandon <txn> | record-install>"
            );
            std::process::exit(2);
        }
    };
    if let Err(error) = result {
        logger::log(&format!("error: {error:#}"));
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
