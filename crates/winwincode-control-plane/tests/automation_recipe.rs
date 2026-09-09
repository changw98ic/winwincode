// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use winwincode_control_plane::automation_recipe::{
    AutomationRecipeError, AutomationRule, AutomationSchedule, DevelopmentRecipeKind,
    DevelopmentRecipePack, RecipeBatch, RecipeBatchItemStatus, RecipeContext, RecipeGate,
    RecipeReference, RecipeReferenceCatalog,
};

fn catalog() -> RecipeReferenceCatalog {
    RecipeReferenceCatalog {
        skills: BTreeSet::new(),
        verification_packs: BTreeSet::from([RecipeReference {
            id: "verification.standard".to_owned(),
            version: 1,
        }]),
    }
}

fn context(kind: DevelopmentRecipeKind) -> RecipeContext {
    RecipeContext {
        kind,
        database_change: false,
        permission_change: false,
        external_write: false,
    }
}

fn input(goal: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("goal".to_owned(), goal.to_owned())])
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end assertion covers the shared recipe and automation contract"
)]
fn six_recipes_schedule_confirm_batch_and_import_fail_closed() {
    let pack = DevelopmentRecipePack::built_in();
    let catalog = catalog();
    pack.validate(&catalog).expect("built-in recipes");
    assert_eq!(pack.recipes.len(), 6);

    for recipe in &pack.recipes {
        let preview = pack
            .preview(
                &recipe.id,
                recipe.version,
                context(recipe.kind),
                &input("Implement the selected change"),
                RecipeGate::Standard,
                &catalog,
            )
            .expect("applicable recipe preview");
        assert!(preview.preview_only);
        assert_eq!(
            pack.preview(
                &recipe.id,
                recipe.version,
                context(recipe.kind),
                &BTreeMap::new(),
                RecipeGate::Standard,
                &catalog,
            ),
            Err(AutomationRecipeError::InvalidInput)
        );
        assert_eq!(
            pack.preview(
                &recipe.id,
                recipe.version,
                context(if recipe.kind == DevelopmentRecipeKind::BugFix {
                    DevelopmentRecipeKind::Refactor
                } else {
                    DevelopmentRecipeKind::BugFix
                }),
                &input("Wrong family"),
                RecipeGate::Standard,
                &catalog,
            ),
            Err(AutomationRecipeError::NotApplicable)
        );
    }

    let elevated = pack
        .preview(
            "page-development",
            1,
            RecipeContext {
                external_write: true,
                ..context(DevelopmentRecipeKind::PageDevelopment)
            },
            &input("Publish the page"),
            RecipeGate::HumanApproval,
            &catalog,
        )
        .expect("risk-escalated preview");
    assert_eq!(elevated.required_gate, RecipeGate::HumanApproval);
    assert!(elevated.risk_reason.contains("external write"));

    let preview = pack
        .preview(
            "bug-fix",
            1,
            context(DevelopmentRecipeKind::BugFix),
            &input("Fix the deterministic failure"),
            RecipeGate::Review,
            &catalog,
        )
        .expect("preview task");
    assert_eq!(
        pack.confirm(&preview, "sha256:changed"),
        Err(AutomationRecipeError::ConfirmationMismatch)
    );
    let confirmed = pack
        .confirm(&preview, &preview.preview_digest)
        .expect("confirm exact preview");

    let mut rule = AutomationRule {
        id: "nightly-bug-fix".to_owned(),
        revision: 1,
        enabled: true,
        recipe_id: "bug-fix".to_owned(),
        recipe_version: 1,
        inputs: input("Fix the deterministic failure"),
        context: context(DevelopmentRecipeKind::BugFix),
        administrator_gate: RecipeGate::Review,
        schedule: AutomationSchedule::Daily {
            time_zone: "America/New_York".to_owned(),
            hour: 1,
            minute: 30,
        },
        ttl_seconds: 3_600,
        max_catch_up: 1,
    };
    let first = pack
        .next_occurrence(&rule, "2026-11-01T04:00:00Z", &catalog)
        .expect("resolve DST-fold occurrence");
    assert_eq!(first.scheduled_at, "2026-11-01T06:30:00Z");
    assert_eq!(first.time_zone, "America/New_York");
    assert_eq!(
        pack.next_occurrence(&rule, "2026-11-01T05:45:00Z", &catalog)
            .expect("keep the later side of a DST fold")
            .scheduled_at,
        first.scheduled_at
    );
    rule.revision = 2;
    let edited = pack
        .next_occurrence(&rule, "2026-11-01T04:00:00Z", &catalog)
        .expect("resolve edited rule");
    assert_eq!(first.occurrence_id, edited.occurrence_id);
    assert_eq!(first.task, edited.task);
    assert_ne!(first.rule_revision, edited.rule_revision);
    let future = pack
        .next_occurrence(&rule, &first.scheduled_at, &catalog)
        .expect("resolve future occurrence");
    assert_ne!(first.occurrence_id, future.occurrence_id);
    assert_ne!(first.task.task_key, future.task.task_key);
    let replay: winwincode_control_plane::automation_recipe::AutomationOccurrence =
        serde_json::from_slice(&serde_json::to_vec(&first).expect("encode occurrence"))
            .expect("restart occurrence");
    assert_eq!(replay, first);

    let mut upgraded_pack = pack.clone();
    upgraded_pack.recipes[0].title = "Changed future title".to_owned();
    assert_eq!(upgraded_pack.recipes[0].title, "Changed future title");
    assert_eq!(confirmed.snapshot.title, "Bug fix");

    let duplicate = RecipeBatch::new(
        "batch-1",
        [
            ("same".to_owned(), confirmed.clone()),
            ("same".to_owned(), confirmed.clone()),
        ],
    );
    assert_eq!(duplicate, Err(AutomationRecipeError::DuplicateBatchItem));
    let mut batch = RecipeBatch::new(
        "batch-2",
        [
            ("one".to_owned(), confirmed.clone()),
            ("two".to_owned(), confirmed.clone()),
            ("three".to_owned(), confirmed),
        ],
    )
    .expect("independent batch");
    let ids = batch.items.keys().cloned().collect::<Vec<_>>();
    batch
        .settle(&ids[0], RecipeBatchItemStatus::Completed)
        .expect("complete first item");
    batch
        .settle(&ids[1], RecipeBatchItemStatus::Failed)
        .expect("fail second item");
    batch.cancel(&ids[2]).expect("cancel third item");
    let statistics = batch.statistics();
    assert_eq!(statistics.len(), 1);
    assert_eq!(statistics[0].completed, 1);
    assert_eq!(statistics[0].failed, 1);
    assert_eq!(statistics[0].cancelled, 1);
    assert!(!statistics[0].ranking_eligible);

    let imported = serde_json::to_vec(&pack).expect("export recipes");
    assert_eq!(
        DevelopmentRecipePack::from_json(&imported, &RecipeReferenceCatalog::default()),
        Err(AutomationRecipeError::MissingReference)
    );
    let mut unsafe_pack: serde_json::Value =
        serde_json::from_slice(&imported).expect("decode exported recipes");
    unsafe_pack["recipes"][0]["parameters"][0]["default"] = serde_json::json!("token=raw-secret");
    assert_eq!(
        DevelopmentRecipePack::from_json(
            &serde_json::to_vec(&unsafe_pack).expect("encode unsafe import"),
            &catalog,
        ),
        Err(AutomationRecipeError::InvalidRecipe)
    );
    unsafe_pack["recipes"][0]["authorizationPolicy"] = serde_json::json!("allow_all");
    assert_eq!(
        DevelopmentRecipePack::from_json(
            &serde_json::to_vec(&unsafe_pack).expect("encode policy override"),
            &catalog,
        ),
        Err(AutomationRecipeError::InvalidJson)
    );
}
