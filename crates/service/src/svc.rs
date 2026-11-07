//! Windows service plumbing for the `Keel` service: dispatch, install, uninstall, restart.
//! The service only ever runs from `C:\Program Files\Keel\keel.exe` (admin-only folder): the
//! installer puts it there; `install` refuses any other copy.
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use windows_service::service::{
    Service, ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceInfo,
    ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

pub const NAME: &str = "Keel";
const DISPLAY: &str = "Keel";
const DESCRIPTION: &str = "Keel: eGPU limits and watchdog, on-demand app families, network routing.";

define_windows_service!(ffi_service_main, service_main);

pub fn dispatch() -> anyhow::Result<()> {
    service_dispatcher::start(NAME, ffi_service_main)?;
    Ok(())
}

fn status(state: ServiceState) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == ServiceState::Running { ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN } else { ServiceControlAccept::empty() },
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(20),
        process_id: None,
    }
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() { crate::log(format!("{e:#}")); }
}

fn run_service() -> anyhow::Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let handle = service_control_handler::register(NAME, move |c| match c {
        ServiceControl::Stop | ServiceControl::Shutdown => { s.store(true, Ordering::SeqCst); ServiceControlHandlerResult::NoError }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    handle.set_service_status(status(ServiceState::Running))?;
    let r = crate::run(&stop);
    handle.set_service_status(status(ServiceState::Stopped))?;
    r
}

/// `C:\Program Files\Keel`: the only place the service runs from.
pub fn install_dir() -> PathBuf {
    PathBuf::from(std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into())).join("Keel")
}

/// Whether a service of that name is registered (any state).
pub fn exists(name: &str) -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(name, ServiceAccess::QUERY_STATUS)).is_ok()
}

fn wait_stopped(svc: &Service) -> anyhow::Result<()> {
    for _ in 0..150 {
        if svc.query_status()?.current_state == ServiceState::Stopped { return Ok(()); }
        std::thread::sleep(Duration::from_millis(200));
    }
    anyhow::bail!("service did not stop within 30 s")
}

/// Registers (or, on upgrade, restarts) the service (admin; run by the installer).
pub fn install() -> anyhow::Result<()> {
    let me = std::env::current_exe()?;
    let want = install_dir().join("keel.exe");
    anyhow::ensure!(me.to_string_lossy().eq_ignore_ascii_case(&want.to_string_lossy()),
        "the service runs only from {} (this is {}); use the installer", want.display(), me.display());
    keel_core::paths::create_data_dir()?;
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    if let Ok(svc) = mgr.open_service(NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::CHANGE_CONFIG) {
        if svc.query_status()?.current_state != ServiceState::Stopped { let _ = svc.stop(); wait_stopped(&svc)?; }
        set_recovery(&svc)?;
        svc.start::<&str>(&[])?;
        println!("service {NAME} already installed; restarted");
        return Ok(());
    }
    let info = ServiceInfo {
        name: NAME.into(),
        display_name: DISPLAY.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: want,
        launch_arguments: vec!["service".into()],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let svc = mgr.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)?;
    svc.set_description(DESCRIPTION)?;
    set_recovery(&svc)?;
    svc.start::<&str>(&[])?;
    println!("service {NAME} installed and started");
    Ok(())
}

/// The features run on threads of one process: a panic is caught per feature, but a native crash
/// (e.g. inside a vendor DLL) ends the process. The SCM restarts it then: 5 s, 30 s, then every
/// 2 minutes; the count resets after a day without a crash.
fn set_recovery(svc: &Service) -> anyhow::Result<()> {
    use windows_service::service::{ServiceAction, ServiceActionType, ServiceFailureActions, ServiceFailureResetPeriod};
    let restart = |s| ServiceAction { action_type: ServiceActionType::Restart, delay: std::time::Duration::from_secs(s) };
    svc.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(std::time::Duration::from_secs(86400)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![restart(5), restart(30), restart(120)]),
    })?;
    Ok(())
}

/// Stops the service, puts back everything its features changed, and removes it (admin).
pub fn uninstall() -> anyhow::Result<()> {
    keel_core::paths::ensure_may_write_data_dir()?;
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let svc = mgr.open_service(NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE).ok();
    if let Some(s) = &svc {
        if s.query_status()?.current_state != ServiceState::Stopped { let _ = s.stop(); wait_stopped(s)?; }
    }
    for l in crate::restore_all() { println!("{l}"); }
    match svc {
        Some(s) => { s.delete()?; println!("service {NAME} removed"); }
        None => println!("service {NAME} not installed"),
    }
    Ok(())
}

pub fn restart() -> anyhow::Result<()> {
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let svc = mgr.open_service(NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::START)?;
    if svc.query_status()?.current_state != ServiceState::Stopped { svc.stop()?; wait_stopped(&svc)?; }
    svc.start::<&str>(&[])?;
    println!("service {NAME} restarted");
    Ok(())
}
