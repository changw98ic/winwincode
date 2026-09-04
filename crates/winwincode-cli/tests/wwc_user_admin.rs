// SPDX-License-Identifier: Apache-2.0

//! End-to-end coverage for the `wwc user` Owner administration surface: the
//! commands must drive the canonical `UserAccountService` on the Server
//! product-state database, reveal the temporary password exactly once, and
//! keep the single-Owner and session-revocation boundaries.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_cli::{WwcCliExit, run_cli};
use winwincode_server::{CredentialRejection, UserAccountService};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    data_directory: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let data_directory = std::env::temp_dir().join(format!(
            "winwincode-cli-user-tests-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&data_directory);
        fs::create_dir_all(&data_directory).expect("fixture data directory");
        Self { data_directory }
    }

    fn data_dir(&self) -> &str {
        self.data_directory.to_str().expect("UTF-8 fixture path")
    }

    fn user(&self, arguments: &[&str]) -> WwcCliExit {
        let mut full = vec!["user"];
        full.extend(arguments.iter().copied());
        full.extend(["--data-dir", self.data_dir()]);
        run_cli(
            &full
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>(),
            &winwincode_cli::SystemLocalLauncher::new(std::env::temp_dir()),
        )
    }

    fn service(&self) -> UserAccountService {
        UserAccountService::open(&self.data_directory).expect("account service")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.data_directory);
    }
}

fn json(outcome: &WwcCliExit) -> serde_json::Value {
    serde_json::from_str(&outcome.stdout)
        .unwrap_or_else(|error| panic!("CLI JSON should parse ({error}): {:?}", outcome.stdout))
}

#[test]
fn create_outputs_a_one_time_password_that_verifies() {
    let fixture = Fixture::new();
    assert_eq!(fixture.user(&["create", "Wen", "--role", "owner"]).code, 0);

    let outcome = fixture.user(&["create", "Ada", "--json"]);
    assert_eq!(outcome.code, 0, "{}", outcome.stderr);
    assert!(outcome.stderr.is_empty());
    let payload = json(&outcome);
    assert_eq!(payload["status"], "user-created");
    assert_eq!(payload["user"]["username"], "Ada");
    assert_eq!(payload["user"]["role"], "member");
    assert_eq!(payload["user"]["state"], "active");
    assert_eq!(payload["user"]["userId"].as_str().unwrap().len(), 30);
    let password = payload["temporaryPassword"].as_str().expect("password");

    let service = fixture.service();
    let account = service
        .verify_credentials("ada", password)
        .expect("lookup")
        .expect("temporary password verifies the stored Argon2id hash");
    assert!(account.password_hash.starts_with("$argon2id$"));
    assert_eq!(account.normalized_username, "ada");

    // The human rendering also reveals the password exactly once, with the
    // one-time warning, and never repeats it in later commands.
    let human = fixture.user(&["disable", "Ada"]);
    assert_eq!(human.code, 0, "{}", human.stderr);
    assert!(!human.stdout.contains(password));
    assert!(!human.stdout.contains("temporaryPassword"));
}

#[test]
fn duplicate_normalized_usernames_conflict_with_a_non_zero_exit() {
    let fixture = Fixture::new();
    assert_eq!(fixture.user(&["create", "Wen", "--role", "owner"]).code, 0);

    let member = fixture.user(&["create", "Ada"]);
    assert_eq!(member.code, 0, "{}", member.stderr);

    let member_again = fixture.user(&["create", "ada"]);
    assert_eq!(member_again.code, 5);
    assert!(member_again.stdout.is_empty());
    assert!(member_again.stderr.contains("user.username-conflict"));

    let owner_again = fixture.user(&["create", "WEN", "--role", "owner"]);
    assert_eq!(owner_again.code, 5);
    assert!(owner_again.stderr.contains("user.owner-exists"));
}

#[test]
fn disable_enable_roundtrip_keeps_the_cas_state_and_reports_session_boundaries() {
    let fixture = Fixture::new();
    let owner = fixture.user(&["create", "Wen", "--role", "owner", "--json"]);
    assert_eq!(owner.code, 0, "{}", owner.stderr);
    let owner_payload = json(&owner);
    let owner_password = owner_payload["temporaryPassword"]
        .as_str()
        .expect("password")
        .to_owned();
    let member = fixture.user(&["create", "Ada", "--json"]);
    assert_eq!(member.code, 0, "{}", member.stderr);
    let member_payload = json(&member);
    let member_password = member_payload["temporaryPassword"]
        .as_str()
        .expect("password")
        .to_owned();

    let disabled = fixture.user(&["disable", "Ada", "--json"]);
    assert_eq!(disabled.code, 0, "{}", disabled.stderr);
    let disabled_payload = json(&disabled);
    assert_eq!(disabled_payload["changed"], serde_json::Value::Bool(true));
    assert_eq!(disabled_payload["user"]["state"], "disabled");

    // The durable state flipped: login now reports the account as disabled.
    let service = fixture.service();
    assert_eq!(
        service
            .verify_credentials("ada", &member_password)
            .expect("lookup"),
        Err(CredentialRejection::AccountDisabled)
    );
    assert!(
        service
            .verify_credentials("wen", &owner_password)
            .expect("lookup")
            .is_ok()
    );

    let enabled = fixture.user(&["enable", "Ada"]);
    assert_eq!(enabled.code, 0, "{}", enabled.stderr);
    assert!(enabled.stdout.contains("用户已启用。"));
    assert!(
        service
            .verify_credentials("ada", &member_password)
            .expect("lookup")
            .is_ok()
    );

    // The human disable rendering carries the session-revocation boundary.
    let human_disable = fixture.user(&["disable", "Ada"]);
    assert_eq!(human_disable.code, 0, "{}", human_disable.stderr);
    assert!(human_disable.stdout.contains("用户已禁用。"));
    assert!(
        human_disable
            .stdout
            .contains("浏览器会话撤销由 Server 负责，CLI 直连数据库路径，不触达在线会话。")
    );
    assert!(
        human_disable
            .stdout
            .contains("需重启 Server 或改经 HTTP 端点操作才即时生效")
    );

    let already = fixture.user(&["disable", "Ada", "--json"]);
    assert_eq!(already.code, 0, "{}", already.stderr);
    assert_eq!(json(&already)["changed"], serde_json::Value::Bool(false));

    let ghost = fixture.user(&["disable", "Ghost"]);
    assert_eq!(ghost.code, 5);
    assert!(ghost.stderr.contains("user.not-found"));
}

#[test]
fn reset_password_replaces_the_old_password_immediately() {
    let fixture = Fixture::new();
    assert_eq!(fixture.user(&["create", "Wen", "--role", "owner"]).code, 0);
    let member = fixture.user(&["create", "Ada", "--json"]);
    let member_payload = json(&member);
    let old_password = member_payload["temporaryPassword"]
        .as_str()
        .expect("password")
        .to_owned();

    let reset = fixture.user(&["reset-password", "Ada", "--json"]);
    assert_eq!(reset.code, 0, "{}", reset.stderr);
    let payload = json(&reset);
    assert_eq!(payload["status"], "password-reset");
    let new_password = payload["temporaryPassword"]
        .as_str()
        .expect("password")
        .to_owned();
    assert_ne!(old_password, new_password);

    let service = fixture.service();
    assert_eq!(
        service
            .verify_credentials("ada", &old_password)
            .expect("lookup"),
        Err(CredentialRejection::BadPassword)
    );
    assert!(
        service
            .verify_credentials("ada", &new_password)
            .expect("lookup")
            .is_ok()
    );

    let human = fixture.user(&["reset-password", "Ada"]);
    assert_eq!(human.code, 0, "{}", human.stderr);
    assert!(human.stdout.contains("新临时密码："));
    assert!(human.stdout.contains("只显示这一次"));
    assert!(human.stdout.contains("原密码立即失效"));
}

#[test]
fn owner_creation_is_allowed_once_and_members_require_initialization() {
    let fixture = Fixture::new();

    // No Owner yet: member creation must point at the one-time
    // initialization paths instead of writing an account.
    let early_member = fixture.user(&["create", "Ada"]);
    assert_eq!(early_member.code, 3);
    assert!(early_member.stderr.is_empty());
    assert!(early_member.stdout.contains("Server 尚未初始化"));
    assert!(early_member.stdout.contains("bootstrap proof"));
    assert!(early_member.stdout.contains("--role owner"));
    let early_reset = fixture.user(&["reset-password", "Ada"]);
    assert_eq!(early_reset.code, 3);

    let first_owner = fixture.user(&["create", "Wen", "--role", "owner", "--json"]);
    assert_eq!(first_owner.code, 0, "{}", first_owner.stderr);
    assert_eq!(json(&first_owner)["user"]["role"], "owner");

    let second_owner = fixture.user(&["create", "Xue", "--role", "owner"]);
    assert_eq!(second_owner.code, 5);
    assert!(second_owner.stderr.contains("user.owner-exists"));

    // With an Owner in place, member creation proceeds normally.
    let member = fixture.user(&["create", "Ada"]);
    assert_eq!(member.code, 0, "{}", member.stderr);
}

#[test]
fn usage_errors_name_the_user_surface() {
    let fixture = Fixture::new();
    let missing_username = fixture.user(&["create"]);
    assert_eq!(missing_username.code, 2);
    assert!(missing_username.stderr.contains("需要且只需要一个用户名"));

    let bad_role = fixture.user(&["create", "Ada", "--role", "admin"]);
    assert_eq!(bad_role.code, 2);
    assert!(bad_role.stderr.contains("--role 只能是 owner 或 member"));

    let unknown_action = fixture.user(&["promote", "Ada"]);
    assert_eq!(unknown_action.code, 2);
    assert!(unknown_action.stderr.contains("未知 user 命令 promote"));

    let help = run_cli(
        &["help".to_owned()],
        &winwincode_cli::SystemLocalLauncher::new(std::env::temp_dir()),
    );
    assert!(help.stdout.contains("wwc user create"));
    assert!(help.stdout.contains("wwc user reset-password"));
    assert!(help.stdout.contains("WWC_SERVER_DATA_DIRECTORY"));
}
