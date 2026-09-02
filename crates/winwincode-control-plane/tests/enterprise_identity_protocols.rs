// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, EnterpriseOrganizationUpdateCommand, EnterpriseOrganizationUpdateCommandCommand,
    EnterpriseOrganizationUpdatePayload, OrganizationScope, OrganizationScopeKind, Scope,
    ServiceAccountActor, ServiceAccountActorKind,
};
use winwincode_control_plane::{
    BrowserSessionLifecycleError, BrowserSessionLifecyclePort,
    CanonicalEnterpriseIdentityLifecycle, EnterpriseIdentityClock, EnterpriseIdentityClockError,
    EnterpriseIdentityLifecyclePort, EnterpriseIdentityProtocolAdapter,
    EnterpriseIdentityProtocolConfig, EnterpriseIdentityService, EnterpriseProtocolClock,
    EnterpriseProtocolClockError, EnterpriseProtocolErrorKind, EnterpriseRbacClock,
    EnterpriseRbacClockError, EnterpriseRbacService, ExternalIdentityProvider,
    ExternalIdentityReference, OidcIdToken, OidcTokenVerifier, ProtocolVerificationError,
    ProvisionExternalUser, SamlResponse, SamlResponseVerifier, ScimBearerToken, ScimBearerVerifier,
    ScimLifecycleEvent, ScimOperation, ScimTeamUpsert, ScimUserDeprovision, ScimUserProvision,
    TrustedProtocolParty, VerifiedOidcClaims, VerifiedSamlClaims, VerifiedScimClient,
};
use winwincode_domain::{
    EnterpriseTeamId, OrganizationId, RequestId, Revision, SchemaVersion, ServiceAccountId,
    Sha256Digest, UserId,
};
use winwincode_storage::{ProductStateStorage, SqliteStorage};

const NOW: u64 = 1_700_000_000_000;
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct FixedClock;

impl EnterpriseIdentityClock for FixedClock {
    fn now_millis(&mut self) -> Result<u64, EnterpriseIdentityClockError> {
        Ok(NOW)
    }
}

impl EnterpriseRbacClock for FixedClock {
    fn now_millis(&mut self) -> Result<u64, EnterpriseRbacClockError> {
        Ok(NOW)
    }
}

impl EnterpriseProtocolClock for FixedClock {
    fn now_millis(&mut self) -> Result<u64, EnterpriseProtocolClockError> {
        Ok(NOW)
    }
}

#[derive(Default)]
struct SessionFacts {
    replaced: Mutex<Vec<Actor>>,
    revoked: Mutex<Vec<Actor>>,
}

impl BrowserSessionLifecyclePort for SessionFacts {
    fn replace_authorized_scopes(
        &self,
        actor: &Actor,
        _authorized_scopes: Vec<Scope>,
    ) -> Result<usize, BrowserSessionLifecycleError> {
        self.replaced
            .lock()
            .expect("replace lock")
            .push(actor.clone());
        Ok(1)
    }

    fn revoke_actor_sessions(&self, actor: &Actor) -> Result<usize, BrowserSessionLifecycleError> {
        self.revoked
            .lock()
            .expect("revoke lock")
            .push(actor.clone());
        Ok(1)
    }
}

struct Fixture {
    root: PathBuf,
    organization_id: OrganizationId,
    user_id: UserId,
    identity: Arc<EnterpriseIdentityService>,
    rbac: Arc<EnterpriseRbacService>,
    sessions: Arc<SessionFacts>,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let serial = NEXT_FIXTURE.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "winwincode-enterprise-protocol-{label}-{}-{serial}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create fixture");
        let organization_id = OrganizationId("org_00000000000000000000000001".to_owned());
        let user_id = UserId("usr_00000000000000000000000001".to_owned());
        let identity = Arc::new(EnterpriseIdentityService::with_clock(
            Box::new(SqliteStorage::open(&root).expect("identity storage")),
            Box::new(FixedClock),
        ));
        let rbac = Arc::new(EnterpriseRbacService::with_clock(
            Box::new(SqliteStorage::open(&root).expect("RBAC storage")),
            Box::new(FixedClock),
        ));
        let fixture = Self {
            root,
            organization_id,
            user_id,
            identity,
            rbac,
            sessions: Arc::new(SessionFacts::default()),
        };
        fixture.seed_organization();
        fixture
    }

    fn seed_organization(&self) {
        self.rbac
            .update_organization(&EnterpriseOrganizationUpdateCommand {
                actor: management_actor(),
                command: EnterpriseOrganizationUpdateCommandCommand::EnterpriseOrganizationUpdate,
                expected_revision: Revision(0),
                payload: EnterpriseOrganizationUpdatePayload {
                    display_name: "Example Organization".to_owned(),
                    organization_id: self.organization_id.clone(),
                    slug: "example".to_owned(),
                    state: "active".to_owned(),
                },
                request_id: request(1),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: Scope::OrganizationScope(organization_scope(&self.organization_id)),
            })
            .expect("seed Organization");
    }

    fn lifecycle(&self) -> CanonicalEnterpriseIdentityLifecycle {
        CanonicalEnterpriseIdentityLifecycle::new(
            Arc::clone(&self.identity),
            Arc::clone(&self.rbac),
            self.sessions.clone(),
            management_actor(),
        )
    }

    fn adapter(&self) -> EnterpriseIdentityProtocolAdapter {
        EnterpriseIdentityProtocolAdapter::with_clock(
            Box::new(SqliteStorage::open(&self.root).expect("protocol storage")),
            Box::new(self.lifecycle()),
            Box::new(FakeOidcVerifier),
            Box::new(FakeSamlVerifier),
            Box::new(FakeScimVerifier),
            Box::new(FixedClock),
            protocol_config(&self.organization_id),
        )
        .expect("protocol adapter")
    }

    fn provision_mapping(&self, provider: ExternalIdentityProvider, operation: &str) {
        self.lifecycle()
            .provision_user(&ProvisionExternalUser {
                operation_id: operation.to_owned(),
                identity: ExternalIdentityReference {
                    organization_id: self.organization_id.clone(),
                    provider,
                    issuer_sha256: sha256(match provider {
                        ExternalIdentityProvider::Oidc => b"https://idp.example/oidc",
                        ExternalIdentityProvider::Saml => b"https://idp.example/saml",
                        ExternalIdentityProvider::Scim => b"https://idp.example/scim",
                    }),
                    subject_sha256: sha256(b"subject-1"),
                },
                user_id: self.user_id.clone(),
                display_name: "Ada Example".to_owned(),
                authorized_scopes: vec![Scope::OrganizationScope(organization_scope(
                    &self.organization_id,
                ))],
                team_ids: Vec::new(),
                role_assignments: Vec::new(),
            })
            .expect("provision mapping");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct FakeOidcVerifier;

impl OidcTokenVerifier for FakeOidcVerifier {
    fn verify(&mut self, token: &str) -> Result<VerifiedOidcClaims, ProtocolVerificationError> {
        if token == "bad-signature" {
            return Err(ProtocolVerificationError::signature_rejected());
        }
        let mut claims = VerifiedOidcClaims {
            issuer: "https://idp.example/oidc".to_owned(),
            audiences: vec!["winwincode".to_owned()],
            subject: "subject-1".to_owned(),
            token_id: "oidc-token-1".to_owned(),
            issued_at_millis: NOW - 1_000,
            not_before_millis: NOW - 1_000,
            expires_at_millis: NOW + 60_000,
        };
        match token {
            "wrong-issuer" => "https://foreign.example".clone_into(&mut claims.issuer),
            "wrong-audience" => claims.audiences = vec!["foreign".to_owned()],
            "expired" => claims.expires_at_millis = NOW - 2_000,
            "changed-replay" => "subject-2".clone_into(&mut claims.subject),
            _ => {}
        }
        Ok(claims)
    }
}

struct FakeSamlVerifier;

impl SamlResponseVerifier for FakeSamlVerifier {
    fn verify(&mut self, response: &[u8]) -> Result<VerifiedSamlClaims, ProtocolVerificationError> {
        if response == b"bad-signature" {
            return Err(ProtocolVerificationError::signature_rejected());
        }
        Ok(VerifiedSamlClaims {
            issuer: "https://idp.example/saml".to_owned(),
            audiences: if response == b"wrong-audience" {
                vec!["foreign".to_owned()]
            } else {
                vec!["winwincode".to_owned()]
            },
            subject: "subject-1".to_owned(),
            assertion_id: "saml-assertion-1".to_owned(),
            issued_at_millis: NOW - 1_000,
            not_before_millis: NOW - 1_000,
            expires_at_millis: NOW + 60_000,
        })
    }
}

struct FakeScimVerifier;

impl ScimBearerVerifier for FakeScimVerifier {
    fn verify(&mut self, bearer: &str) -> Result<VerifiedScimClient, ProtocolVerificationError> {
        if bearer != "scim-valid" {
            return Err(ProtocolVerificationError::signature_rejected());
        }
        Ok(VerifiedScimClient {
            issuer: "https://idp.example/scim".to_owned(),
            audiences: vec!["winwincode-scim".to_owned()],
            client_id: "scim-client-1".to_owned(),
            expires_at_millis: NOW + 60_000,
        })
    }
}

#[test]
fn oidc_and_saml_validate_trust_replay_restart_and_current_rbac() {
    let fixture = Fixture::new("login");
    fixture.provision_mapping(ExternalIdentityProvider::Oidc, "provision-oidc");
    fixture.provision_mapping(ExternalIdentityProvider::Saml, "provision-saml");
    let adapter = fixture.adapter();

    let Err(error) = OidcIdToken::new("invalid token") else {
        panic!("compact token whitespace accepted");
    };
    assert_eq!(error.kind(), EnterpriseProtocolErrorKind::InvalidRequest);
    assert_eq!(
        adapter
            .authenticate_oidc(&OidcIdToken::new("bad-signature").expect("token"))
            .expect_err("signature rejection")
            .kind(),
        EnterpriseProtocolErrorKind::SignatureRejected
    );
    assert_eq!(
        adapter
            .authenticate_oidc(&OidcIdToken::new("wrong-issuer").expect("token"))
            .expect_err("issuer rejection")
            .kind(),
        EnterpriseProtocolErrorKind::IssuerMismatch
    );
    assert_eq!(
        adapter
            .authenticate_oidc(&OidcIdToken::new("wrong-audience").expect("token"))
            .expect_err("audience rejection")
            .kind(),
        EnterpriseProtocolErrorKind::AudienceMismatch
    );
    assert_eq!(
        adapter
            .authenticate_oidc(&OidcIdToken::new("expired").expect("token"))
            .expect_err("expiry rejection")
            .kind(),
        EnterpriseProtocolErrorKind::Expired
    );

    let first = adapter
        .authenticate_oidc(&OidcIdToken::new("valid").expect("token"))
        .expect("OIDC login");
    assert!(!first.idempotent_replay);
    assert_eq!(first.principal.actor, user_actor(&fixture.user_id));
    let replay = fixture
        .adapter()
        .authenticate_oidc(&OidcIdToken::new("valid").expect("token"))
        .expect("restart replay");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.principal, first.principal);
    assert_eq!(
        adapter
            .authenticate_oidc(&OidcIdToken::new("changed-replay").expect("token"))
            .expect_err("changed token id replay")
            .kind(),
        EnterpriseProtocolErrorKind::ReplayConflict
    );

    assert_eq!(
        adapter
            .authenticate_saml(
                &SamlResponse::new(b"bad-signature".to_vec()).expect("SAML response"),
            )
            .expect_err("SAML signature rejection")
            .kind(),
        EnterpriseProtocolErrorKind::SignatureRejected
    );
    let saml = adapter
        .authenticate_saml(&SamlResponse::new(b"valid".to_vec()).expect("SAML response"))
        .expect("SAML login");
    assert!(!saml.idempotent_replay);
    assert_eq!(saml.principal.actor, user_actor(&fixture.user_id));
    assert!(
        fixture
            .adapter()
            .authenticate_saml(&SamlResponse::new(b"valid".to_vec()).expect("SAML response"))
            .expect("SAML restart replay")
            .idempotent_replay
    );
    assert_eq!(
        adapter
            .authenticate_saml(
                &SamlResponse::new(b"wrong-audience".to_vec()).expect("SAML response"),
            )
            .expect_err("SAML audience rejection")
            .kind(),
        EnterpriseProtocolErrorKind::AudienceMismatch
    );
}

#[test]
fn scim_user_and_team_lifecycle_is_ordered_idempotent_audited_and_revokes() {
    let fixture = Fixture::new("scim");
    fixture.provision_mapping(ExternalIdentityProvider::Oidc, "provision-oidc");
    let adapter = fixture.adapter();
    let bearer = ScimBearerToken::new("scim-valid").expect("SCIM bearer");
    apply_scim_user_replay_cases(&fixture, &adapter, &bearer);
    apply_scim_team_lifecycle(&fixture, &adapter, &bearer);
    deprovision_scim_user(&fixture, &adapter, &bearer);

    let inspection = SqliteStorage::open(&fixture.root).expect("audit inspection");
    let audit = inspection
        .pending_audit_events()
        .expect("pending audit events")
        .into_iter()
        .map(|event| String::from_utf8(event.payload().to_vec()).expect("audit UTF-8"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(audit.contains("scim.user.provision"));
    assert!(audit.contains("scim.user.deprovision"));
    assert!(audit.contains("scim.team.deprovision"));
    assert!(!audit.contains("scim-valid"));
}

fn apply_scim_user_replay_cases(
    fixture: &Fixture,
    adapter: &EnterpriseIdentityProtocolAdapter,
    bearer: &ScimBearerToken,
) {
    let provision = ScimLifecycleEvent {
        event_id: "scim-user-1".to_owned(),
        sequence: 1,
        operation: ScimOperation::ProvisionUser(ScimUserProvision {
            external_subject: "subject-1".to_owned(),
            user_id: fixture.user_id.clone(),
            display_name: "Ada Example".to_owned(),
            authorized_scopes: vec![Scope::OrganizationScope(organization_scope(
                &fixture.organization_id,
            ))],
            team_ids: Vec::new(),
            role_assignments: Vec::new(),
        }),
    };
    assert_eq!(
        adapter
            .apply_scim(
                &ScimBearerToken::new("scim-bad").expect("bad SCIM bearer"),
                &provision,
            )
            .expect_err("SCIM bearer signature rejection")
            .kind(),
        EnterpriseProtocolErrorKind::SignatureRejected
    );
    let first = adapter
        .apply_scim(bearer, &provision)
        .expect("provision user");
    assert_eq!(
        adapter
            .apply_scim(bearer, &provision)
            .expect("exact replay"),
        first
    );
    let mut changed = provision.clone();
    let ScimOperation::ProvisionUser(changed_user) = &mut changed.operation else {
        panic!("user operation");
    };
    "Changed".clone_into(&mut changed_user.display_name);
    assert_eq!(
        adapter
            .apply_scim(bearer, &changed)
            .expect_err("changed replay")
            .kind(),
        EnterpriseProtocolErrorKind::ReplayConflict
    );
    let mut out_of_order = provision.clone();
    "scim-user-old".clone_into(&mut out_of_order.event_id);
    assert_eq!(
        adapter
            .apply_scim(bearer, &out_of_order)
            .expect_err("out of order")
            .kind(),
        EnterpriseProtocolErrorKind::OutOfOrder
    );

    let mut update = provision;
    "scim-user-update".clone_into(&mut update.event_id);
    update.sequence = 2;
    let ScimOperation::ProvisionUser(updated_user) = &mut update.operation else {
        panic!("user operation");
    };
    "Ada Updated".clone_into(&mut updated_user.display_name);
    adapter.apply_scim(bearer, &update).expect("update user");
    assert_eq!(
        fixture
            .rbac
            .membership_by_actor(&user_actor(&fixture.user_id), &fixture.organization_id)
            .expect("load updated Membership")
            .expect("updated Membership")
            .display_name,
        "Ada Updated"
    );
}

fn apply_scim_team_lifecycle(
    fixture: &Fixture,
    adapter: &EnterpriseIdentityProtocolAdapter,
    bearer: &ScimBearerToken,
) {
    let team_id = EnterpriseTeamId("tem_00000000000000000000000001".to_owned());
    let team = ScimLifecycleEvent {
        event_id: "scim-team-1".to_owned(),
        sequence: 1,
        operation: ScimOperation::UpsertTeam(ScimTeamUpsert {
            team_id: team_id.clone(),
            display_name: "Reviewers".to_owned(),
            state: "active".to_owned(),
            role_assignments: Vec::new(),
        }),
    };
    adapter.apply_scim(bearer, &team).expect("create Team");
    let mut archive = team;
    "scim-team-2".clone_into(&mut archive.event_id);
    archive.sequence = 2;
    let ScimOperation::UpsertTeam(team) = &mut archive.operation else {
        panic!("team operation");
    };
    "archived".clone_into(&mut team.state);
    adapter.apply_scim(bearer, &archive).expect("archive Team");
    assert_eq!(
        fixture
            .rbac
            .team(&fixture.organization_id, &team_id)
            .expect("load Team")
            .expect("Team")
            .state,
        "disabled"
    );
}

fn deprovision_scim_user(
    fixture: &Fixture,
    adapter: &EnterpriseIdentityProtocolAdapter,
    bearer: &ScimBearerToken,
) {
    let deprovision = ScimLifecycleEvent {
        event_id: "scim-user-2".to_owned(),
        sequence: 3,
        operation: ScimOperation::DeprovisionUser(ScimUserDeprovision {
            external_subject: "subject-1".to_owned(),
            user_id: fixture.user_id.clone(),
        }),
    };
    adapter
        .apply_scim(bearer, &deprovision)
        .expect("deprovision user");
    let actor = user_actor(&fixture.user_id);
    assert_eq!(
        fixture
            .rbac
            .membership_by_actor(&actor, &fixture.organization_id)
            .expect("load Membership")
            .expect("Membership")
            .state,
        "disabled"
    );
    assert!(
        fixture
            .sessions
            .revoked
            .lock()
            .expect("revoke facts")
            .contains(&actor)
    );
    assert_eq!(
        adapter
            .authenticate_oidc(&OidcIdToken::new("valid").expect("token"))
            .expect_err("RBAC revocation is immediate")
            .kind(),
        EnterpriseProtocolErrorKind::LifecycleRejected
    );
}

fn protocol_config(organization_id: &OrganizationId) -> EnterpriseIdentityProtocolConfig {
    EnterpriseIdentityProtocolConfig {
        organization_id: organization_id.clone(),
        management_actor: management_actor(),
        oidc: TrustedProtocolParty {
            issuer: "https://idp.example/oidc".to_owned(),
            audience: "winwincode".to_owned(),
        },
        saml: TrustedProtocolParty {
            issuer: "https://idp.example/saml".to_owned(),
            audience: "winwincode".to_owned(),
        },
        scim: TrustedProtocolParty {
            issuer: "https://idp.example/scim".to_owned(),
            audience: "winwincode-scim".to_owned(),
        },
        max_clock_skew_millis: 1_000,
        max_assertion_age_millis: 300_000,
    }
}

fn management_actor() -> Actor {
    Actor::ServiceAccountActor(ServiceAccountActor {
        id: ServiceAccountId("svc_00000000000000000000000001".to_owned()),
        kind: ServiceAccountActorKind::ServiceAccount,
    })
}

fn user_actor(user_id: &UserId) -> Actor {
    Actor::UserActor(winwincode_domain::UserActor {
        id: user_id.clone(),
        kind: winwincode_domain::UserActorKind::User,
    })
}

fn organization_scope(organization_id: &OrganizationId) -> OrganizationScope {
    OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: organization_id.clone(),
    }
}

fn request(number: u64) -> RequestId {
    RequestId(format!("req_{number:026}"))
}

fn sha256(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}
