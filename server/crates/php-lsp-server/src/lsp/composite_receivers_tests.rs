use super::*;

#[test]
fn legacy_index_projection_never_claims_a_first_composite_class() {
    let index = WorkspaceIndex::new();
    for ty in [
        TypeInfo::Union(vec![
            TypeInfo::Simple("A".into()),
            TypeInfo::Simple("B".into()),
        ]),
        TypeInfo::Intersection(vec![
            TypeInfo::Simple("A".into()),
            TypeInfo::Simple("B".into()),
        ]),
    ] {
        assert!(
            type_info_fqn_from_index(&index, "Owner", "", &ty).is_none(),
            "legacy projection narrowed {ty}"
        );
    }
}
