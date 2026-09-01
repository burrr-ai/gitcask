//! `gitcask` — the full CLI (serve | compact | repo | wal | synth | import | config).
fn main() -> anyhow::Result<()> {
    gitcask_cli::main()
}
