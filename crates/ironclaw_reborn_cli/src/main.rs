mod cli;
mod commands;
mod context;
mod dto;
mod file_write;
mod operator_env;
mod render;
mod runtime;
mod serve_invocation;
mod webui_token;

fn main() -> anyhow::Result<()> {
    // Query compiled requester protocol before dotenv/operator_env/cli/runtime bootstrap.
    let mut protocol_args=std::env::args_os().skip(1);
    if protocol_args.next().as_deref()==Some(std::ffi::OsStr::new("--print-reborn-egress-protocol")) {
        if protocol_args.next().is_some() {anyhow::bail!("requesterProtocolUnexpectedArgument");}
        return ironclaw_reborn_composition::print_reborn_requester_egress_protocol(env!("CARGO_PKG_VERSION")).map_err(|code|anyhow::anyhow!(code));
    }

    // Mirror the v1 binary's behavior so dev workflows can keep LLM
    // keys / base URLs in `.env`. Silent on missing file — production
    // hosts use shell-exported env or systemd unit env, not `.env` —
    // but any other error (parse failure, permission denied) is
    // surfaced to stderr so a malformed file does not boot the host
    // with stale env. The boot itself still proceeds because
    // operators may have already exported the same keys in their
    // shell.
    if let Err(error) = dotenvy::dotenv()
        && !error.not_found()
    {
        eprintln!("warning: failed to load .env: {error}");
    }
    cli::run()
}
