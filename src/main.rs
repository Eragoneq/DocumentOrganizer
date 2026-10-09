#[cfg(feature = "desktop")]
mod ui;

fn main() -> anyhow::Result<()> {
    #[cfg(feature = "desktop")]
    return ui::run();
    #[cfg(not(feature = "desktop"))]
    anyhow::bail!(
        "Enable the desktop feature to run the application; core tests can run without it"
    )
}
