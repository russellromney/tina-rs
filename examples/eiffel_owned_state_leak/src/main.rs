use eiffel_owned_state_leak::{Report, tina_impl};

fn main() -> anyhow::Result<()> {
    let r: Report = tina_impl::run()?;
    println!(
        "comparison=eiffel_owned_state_leak side=tina documented_compile_fails={} \
         intentional_escape_writes={} exit_clean={}",
        r.documented_compile_fails, r.intentional_escape_writes, r.exit_clean,
    );
    Ok(())
}
