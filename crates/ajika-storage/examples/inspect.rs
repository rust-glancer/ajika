//! Inspect a closed cache without modifying it.

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: inspect <directory>"))?;
    println!(
        "{:#?}",
        ajika_storage::Statistics::inspect(std::path::Path::new(&path))?
    );
    Ok(())
}
