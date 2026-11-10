//! Waiting for the user or the service manager to stop a process.

use std::future::Future;

/// Resolves on Ctrl+C, or on SIGTERM on Unix (what `brew services`,
/// systemd and `kill` send). The handlers are installed when this function
/// is called, so a signal arriving before the future is awaited is not lost.
pub fn shutdown_signal() -> std::io::Result<impl Future<Output = ()>> {
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    Ok(async move {
        #[cfg(unix)]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn sigterm_resolves_the_shutdown_future() {
        let stop = shutdown_signal().unwrap();
        let status =
            std::process::Command::new("kill").args(["-TERM", &std::process::id().to_string()]).status().unwrap();
        assert!(status.success());
        tokio::time::timeout(Duration::from_secs(5), stop).await.expect("SIGTERM was not seen");
    }
}
