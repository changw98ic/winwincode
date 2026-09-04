mod cli;
mod community_gate;
mod doctor;
mod git;
mod launcher;
mod model;
mod user_admin;

pub use cli::{WwcCliExit, render_help, run_cli};
pub use community_gate::{
    COMMUNITY_LOCAL_GATE_SCHEMA_VERSION, CommunityGateFailureCategory, CommunityGateFailureCode,
    CommunityLocalEnvironment, CommunityLocalGateError, CommunityLocalGateReceipt,
    CommunityLocalGateRequest, CommunityLocalSourceTrace, run_community_local_gate,
};
pub use launcher::{LocalLauncherPort, SystemLocalLauncher, default_state_root};
pub use model::{
    AttachRequest, Attachment, AttachmentOutcome, BaselineChoice, BaselineSource,
    DiagnosticCategory, DiagnosticCheck, DiagnosticReport, DiagnosticStatus, DoctorRequest,
    InitRequest, LauncherError, RepositoryInspection, SetupOutcome,
};
pub use user_admin::{
    UserAccountAdmin, UserAccountView, UserAdminError, UserAdminOutcome,
    generate_temporary_password,
};
