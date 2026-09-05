mod backup;
mod cli;
mod community_gate;
mod device_admin;
mod doctor;
mod git;
mod launcher;
mod model;
mod repo_admin;
mod user_admin;

pub use backup::{BACKUP_HELP_LINES, BackupOutcome, run_backup};
pub use cli::{WwcCliExit, render_help, run_cli};
pub use community_gate::{
    COMMUNITY_LOCAL_GATE_SCHEMA_VERSION, CommunityGateFailureCategory, CommunityGateFailureCode,
    CommunityLocalEnvironment, CommunityLocalGateError, CommunityLocalGateReceipt,
    CommunityLocalGateRequest, CommunityLocalSourceTrace, run_community_local_gate,
};
pub use device_admin::{
    ConnectCodeView, DeviceAdminError, DeviceAdminOutcome, DeviceStatusView, device_status,
    refresh_device_connect_code, set_device_lock,
};
pub use launcher::{LocalLauncherPort, SystemLocalLauncher, default_state_root};
pub use model::{
    AttachRequest, Attachment, AttachmentOutcome, BaselineChoice, BaselineSource,
    DiagnosticCategory, DiagnosticCheck, DiagnosticReport, DiagnosticStatus, DoctorRequest,
    InitRequest, LauncherError, RepositoryInspection, SetupOutcome,
};
pub use repo_admin::{
    RepoAdminError, RepoAdminOutcome, RepositoryBindingView, repo_add, repo_list, repo_remove,
};
pub use user_admin::{
    UserAccountAdmin, UserAccountView, UserAdminError, UserAdminOutcome,
    generate_temporary_password,
};
