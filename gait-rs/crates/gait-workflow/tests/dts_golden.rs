//! Golden-file tests for the generated `gait.d.ts`
//! (`gait workflow types`, via `gait_workflow::types::render_dts`).

use gait_workflow::types::render_dts;

const GOLDEN: &str = include_str!("fixtures/gait.d.ts");

const REGENERATE_HINT: &str = "the golden fixture is stale; regenerate it with \
UPDATE_DTS_GOLDEN=1 cargo test -p gait-workflow --test tests dts_golden";

/// The rendered declarations match the checked-in golden fixture byte for
/// byte. With `UPDATE_DTS_GOLDEN=1` the test rewrites the fixture instead of
/// asserting (used only to refresh the golden after an intentional change).
#[test]
fn dts_matches_golden_file() {
    let rendered = render_dts();
    if std::env::var("UPDATE_DTS_GOLDEN").is_ok() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/gait.d.ts");
        std::fs::write(path, &rendered).expect("write golden fixture");
        eprintln!("wrote {path}");
        return;
    }
    assert_eq!(rendered, GOLDEN, "{REGENERATE_HINT}");
}

/// Every schema property name of each option struct appears in the rendered
/// output — the renderer cannot silently drop a field.
#[test]
fn dts_names_every_option_field() {
    let rendered = render_dts();
    macro_rules! check_fields {
        ($($t:ty),+ $(,)?) => {
            $({
                let schema = schemars::schema_for!($t);
                let mut field_count = 0usize;
                if let Some(object) = &schema.schema.object {
                    for property in object.properties.keys() {
                        field_count += 1;
                        assert!(
                            rendered.contains(property),
                            "field `{property}` of {} missing from the generated d.ts",
                            stringify!($t)
                        );
                    }
                }
                assert!(field_count > 0, "{} has no fields", stringify!($t));
            })+
        };
    }
    check_fields!(
        gait_workflow::step::ConfigOpts,
        gait_workflow::step::RecordOpts,
        gait_workflow::step::RecordCommandOpts,
        gait_workflow::step::RetryOpts,
        gait_workflow::step::StepHooks,
        gait_workflow::step::TurnOpts,
        gait_workflow::step::ShellOpts,
        gait_workflow::step::GateOpts,
        gait_workflow::step::StepResult,
    );
}
