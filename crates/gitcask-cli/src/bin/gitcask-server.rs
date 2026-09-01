//! `gitcask-server --config gitcask.toml` — the standalone server (D39): exactly `gitcask serve`,
//! under the name a single-binary deployment expects.
fn main() -> anyhow::Result<()> {
    gitcask_cli::main_server()
}
