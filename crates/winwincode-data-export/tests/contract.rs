use winwincode_data_export::{
    WINWINCODE_EXPORT_FORMAT, WinWinCodeExport, WinWinCodeExportContent, WinWinCodeExportError,
    WinWinCodeExportOrganization, WinWinCodeExportProject,
};

const PUBLISHED_FIXTURE: &[u8] =
    include_bytes!("../../../schema/winwincode-export/v1/winwincode-export.example.json.bytes");

#[test]
fn published_cross_product_fixture_is_the_exact_canonical_document() {
    let export = WinWinCodeExport::decode_canonical(PUBLISHED_FIXTURE).expect("published fixture");
    assert_eq!(export.export_id(), "export_fixture_01");
    assert_eq!(
        export.content_sha256(),
        "04496f3ee1c61033b240f5711ddadb2bf8d4419e074e4b2aec5405c9b8dd2e2d"
    );
    assert_eq!(export.domain_counts().organizations, 1);
    assert_eq!(export.domain_counts().projects, 1);
}

fn organization(id: &str, slug: &str) -> WinWinCodeExportOrganization {
    WinWinCodeExportOrganization {
        source_organization_id: id.to_owned(),
        slug: slug.to_owned(),
        display_name: id.to_owned(),
    }
}

fn project(organization_id: &str, project_id: &str, slug: &str) -> WinWinCodeExportProject {
    WinWinCodeExportProject {
        source_project_id: project_id.to_owned(),
        source_organization_id: organization_id.to_owned(),
        slug: slug.to_owned(),
        display_name: project_id.to_owned(),
    }
}

fn unordered_content() -> WinWinCodeExportContent {
    WinWinCodeExportContent {
        profile_display_name: "Local profile".to_owned(),
        organizations: vec![
            organization("org_00000000000000000000000002", "organization-2"),
            organization("org_00000000000000000000000001", "organization-1"),
        ],
        projects: vec![
            project(
                "org_00000000000000000000000002",
                "prj_00000000000000000000000002",
                "project-2",
            ),
            project(
                "org_00000000000000000000000001",
                "prj_00000000000000000000000001",
                "project-1",
            ),
        ],
    }
}

#[test]
fn construction_sorts_records_and_freezes_canonical_bytes() {
    let first = WinWinCodeExport::try_new("export-001", unordered_content()).expect("export");
    let mut reversed = unordered_content();
    reversed.organizations.reverse();
    reversed.projects.reverse();
    let second = WinWinCodeExport::try_new("export-001", reversed).expect("same export");

    let first_bytes = first.encode_canonical().expect("canonical bytes");
    assert_eq!(
        first_bytes,
        second.encode_canonical().expect("repeat bytes")
    );
    assert_eq!(
        WinWinCodeExport::decode_canonical(&first_bytes).expect("canonical decode"),
        first
    );
    assert_eq!(first.content().organizations[0].slug, "organization-1");
    assert_eq!(first.content().projects[0].slug, "project-1");
    assert_eq!(first.domain_counts().organizations, 2);
    assert_eq!(first.domain_counts().projects, 2);
    assert_eq!(WINWINCODE_EXPORT_FORMAT, "winwincode-export/v1");
    assert_eq!(first.content_sha256().len(), 64);
}

#[test]
fn record_order_compares_unicode_scalar_values() {
    let mut content = WinWinCodeExportContent {
        profile_display_name: "Profile".to_owned(),
        organizations: vec![
            organization("🦀", "astral"),
            organization("\u{e000}", "bmp"),
        ],
        projects: vec![
            project("🦀", "🦀", "astral"),
            project("\u{e000}", "\u{e000}", "bmp"),
        ],
    };
    content.organizations.reverse();

    let export = WinWinCodeExport::try_new("export-unicode-order", content).expect("export");
    assert_eq!(
        export.content().organizations[0].source_organization_id,
        "\u{e000}"
    );
    assert_eq!(
        export.content().organizations[1].source_organization_id,
        "🦀"
    );
    assert_eq!(export.content().projects[0].source_project_id, "\u{e000}");
    assert_eq!(export.content().projects[1].source_project_id, "🦀");
}

#[test]
fn digest_changes_and_noncanonical_bytes_fail_closed() {
    let first = WinWinCodeExport::try_new("export-001", unordered_content()).expect("export");
    let mut changed = unordered_content();
    changed.profile_display_name = "Changed profile".to_owned();
    let changed = WinWinCodeExport::try_new("export-001", changed).expect("changed export");
    assert_ne!(first.content_sha256(), changed.content_sha256());

    let mut tampered: serde_json::Value =
        serde_json::from_slice(&first.encode_canonical().expect("canonical bytes"))
            .expect("JSON value");
    tampered["content"]["profileDisplayName"] = "Changed profile".into();
    assert_eq!(
        WinWinCodeExport::decode_canonical(
            &serde_json::to_vec(&tampered).expect("tampered serialization")
        ),
        Err(WinWinCodeExportError::DigestMismatch)
    );

    let spaced = [
        b" ".as_slice(),
        first
            .encode_canonical()
            .expect("canonical bytes")
            .as_slice(),
    ]
    .concat();
    assert_eq!(
        WinWinCodeExport::decode_canonical(&spaced),
        Err(WinWinCodeExportError::NonCanonical)
    );
}

#[test]
fn equivalent_noncanonical_string_spellings_are_rejected() {
    let canonical = String::from_utf8(PUBLISHED_FIXTURE.to_vec()).expect("UTF-8 fixture");
    for noncanonical in [
        canonical.replacen("Local /", r"Local \/", 1),
        canonical.replacen('é', r"\u00e9", 1),
        canonical.replacen('🦀', r"\ud83e\udd80", 1),
        canonical.replacen(r#"quote \""#, r"quote \u0022", 1),
        canonical.replacen("backslash \\\\", r"backslash \u005c", 1),
    ] {
        let canonical_value: serde_json::Value =
            serde_json::from_slice(PUBLISHED_FIXTURE).expect("canonical value");
        let noncanonical_value: serde_json::Value =
            serde_json::from_str(&noncanonical).expect("equivalent JSON value");
        assert_eq!(noncanonical_value, canonical_value);
        assert_eq!(
            WinWinCodeExport::decode_canonical(noncanonical.as_bytes()),
            Err(WinWinCodeExportError::NonCanonical)
        );
    }
}

#[test]
fn duplicate_references_and_local_paths_are_rejected() {
    let mut duplicate = unordered_content();
    duplicate
        .organizations
        .push(duplicate.organizations[0].clone());
    assert_eq!(
        WinWinCodeExport::try_new("export-001", duplicate),
        Err(WinWinCodeExportError::DuplicateSourceIdentifier)
    );

    let mut unknown = unordered_content();
    unknown.projects[0].source_organization_id = "org_missing".to_owned();
    assert_eq!(
        WinWinCodeExport::try_new("export-001", unknown),
        Err(WinWinCodeExportError::UnknownSourceOrganization)
    );

    for local_path in [
        "/Users/person/private",
        "~/private",
        r"C:\Users\person\private",
        r"\\server\private",
        "file:///home/person/private",
    ] {
        let mut content = unordered_content();
        content.profile_display_name = local_path.to_owned();
        assert_eq!(
            WinWinCodeExport::try_new("export-001", content),
            Err(WinWinCodeExportError::LocalPathNotAllowed)
        );
    }
}

#[test]
fn unicode_code_points_and_ecmascript_boundary_whitespace_match_the_schema() {
    for accepted in ["世界".to_owned(), "🦀".repeat(256)] {
        let mut content = unordered_content();
        content.profile_display_name = accepted;
        WinWinCodeExport::try_new("export-unicode", content).expect("valid Unicode boundary");
    }

    let mut too_many_code_points = unordered_content();
    too_many_code_points.profile_display_name = "🦀".repeat(257);
    assert_eq!(
        WinWinCodeExport::try_new("export-unicode", too_many_code_points),
        Err(WinWinCodeExportError::InvalidContent)
    );

    for whitespace in [' ', '\u{a0}', '\u{3000}', '\u{feff}'] {
        for display_name in [
            format!("{whitespace}Profile"),
            format!("Profile{whitespace}"),
        ] {
            let mut content = unordered_content();
            content.profile_display_name = display_name;
            assert_eq!(
                WinWinCodeExport::try_new("export-unicode", content),
                Err(WinWinCodeExportError::InvalidContent)
            );
        }
    }
}
