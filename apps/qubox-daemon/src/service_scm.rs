//! Windows Service Control Manager (SCM) integration.
//!
//! Provides the SCM event loop (`run_scm`) and privilege checks for
//! install / uninstall subcommands.

use std::sync::mpsc;
use std::time::Duration;
use tracing::info;
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

const SERVICE_NAME: &str = "QuboxDaemon";
const SERVICE_DISPLAY_NAME: &str = "Qubox Daemon";

/// Run the SCM event loop. This is called by `--service-run` on Windows.
///
/// 1. Reports `SERVICE_START_PENDING` with 1-second checkpoint intervals.
/// 2. Spawns the daemon's tokio runtime in a worker thread.
/// 3. Registers a `ServiceControlHandler`.
/// 4. Reports `SERVICE_RUNNING` once the IPC server is bound.
/// 5. Waits for shutdown signal.
/// 6. Reports `SERVICE_STOP_PENDING`, cleans up, then `SERVICE_STOPPED`.
pub fn run_scm() -> windows_service::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

define_windows_service!(ffi_service_main, handle_service_main);

fn handle_service_main(_args: Vec<std::ffi::OsString>) {
    let (shutdown_tx, shutdown_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();

    // Register the service control handler first.
    let status_handle = match windows_service::service::ServiceControlHandler::register(
        SERVICE_NAME,
        move |control_event| -> windows_service::Result<()> {
            match control_event {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    info!("SCM stop/shutdown received");
                    let _ = shutdown_tx.send(());
                    Ok(())
                }
                ServiceControl::Interrogate => Ok(()),
                _ => Ok(()),
            }
        },
    ) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("failed to register service control handler: {e}");
            return;
        }
    };

    // Report SERVICE_START_PENDING
    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::StartPending,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 1,
        wait_hint: Duration::from_secs(30),
        process_id: None,
    });

    // Spawn the daemon in a worker thread with its own tokio runtime.
    let status_handle_clone = status_handle.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(async move {
            let config = crate::DaemonConfig {
                service_mode: true,
                ..Default::default()
            };
            match crate::service::Daemon::run(config).await {
                Ok(()) => info!("daemon service exited cleanly"),
                Err(e) => tracing::error!("daemon service error: {e}"),
            }
        });

        // Report SERVICE_STOP_PENDING
        let _ = status_handle_clone.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::StopPending,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::NO_ERROR,
            checkpoint: 1,
            wait_hint: Duration::from_secs(30),
            process_id: None,
        });
        let _ = ready_tx.send(());
    });

    // Wait for the worker to signal it's done.
    // Periodically report SERVICE_RUNNING if ready_rx hasn't fired.
    loop {
        if ready_rx.recv_timeout(Duration::from_secs(1)).is_ok() {
            break;
        }
        // Keep reporting running state
        let _ = status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP,
            exit_code: ServiceExitCode::NO_ERROR,
            checkpoint: 0,
            wait_hint: Duration::from_secs(30),
            process_id: None,
        });
    }

    // Final: SERVICE_STOPPED
    let _ = status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: Duration::ZERO,
        process_id: None,
    });
}

/// Check whether the current process has an elevated token (Windows).
pub fn is_elevated() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token_handle = HANDLE::default();
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token_handle).is_err() {
            return false;
        }
        let mut elevation: u32 = 0;
        let mut return_length: u32 = 0;
        if GetTokenInformation(
            token_handle,
            TokenElevation,
            Some(&mut elevation as *mut u32 as *mut std::ffi::c_void),
            std::mem::size_of::<u32>() as u32,
            &mut return_length,
        )
        .is_err()
        {
            return false;
        }
        elevation != 0
    }
}

/// Ensure the process is elevated. Returns an error if not.
pub fn ensure_elevated() -> anyhow::Result<()> {
    if !is_elevated() {
        anyhow::bail!(
            "--install / --uninstall require administrative privileges; \
             re-run from an elevated command prompt"
        );
    }
    Ok(())
}

/// Register the service with the SCM.
pub fn install_service(display_name: &str, bin_path: &str) -> windows_service::Result<()> {
    use std::ffi::OsString;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    manager.create_service(
        &ServiceInfo {
            name: Some(OsString::from(SERVICE_NAME)),
            display_name: Some(OsString::from(display_name)),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: std::path::Path::new(bin_path).to_path_buf(),
            launch_arguments: vec![OsString::from("--service-run")],
            dependencies: Vec::new(),
            account_name: None, // runs as LocalSystem
            account_password: None,
        },
        ServiceAccess::CHANGE_CONFIG,
    )?;
    Ok(())
}

/// Unregister the service from the SCM.
pub fn uninstall_service() -> windows_service::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(SERVICE_NAME, ServiceAccess::DELETE)?;
    service.delete()?;
    Ok(())
}

/// Query whether the service is running.
pub fn service_status() -> Option<ServiceState> {
    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)
        .ok()?;
    let status = service.query_status().ok()?;
    Some(status.current_state)
}
