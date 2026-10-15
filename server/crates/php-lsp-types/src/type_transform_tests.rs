use super::*;

#[test]
fn transform_preserves_non_type_shape_metadata_and_visits_replacements_once() {
    let ty = TypeInfo::ArrayShape(vec![ArrayShapeItem {
        key: Some("self".into()),
        optional: true,
        value: TypeInfo::Nullable(Box::new(TypeInfo::Self_)),
    }]);
    let mut count = 0;
    let result = try_transform_type(&ty, &mut |node| {
        count += 1;
        Some(if node == TypeInfo::Self_ {
            TypeInfo::Simple("\\Owner".into())
        } else {
            node
        })
    })
    .unwrap();
    assert_eq!(count, 3);
    assert_eq!(
        result,
        TypeInfo::ArrayShape(vec![ArrayShapeItem {
            key: Some("self".into()),
            optional: true,
            value: TypeInfo::Nullable(Box::new(TypeInfo::Simple("\\Owner".into())))
        }])
    );
    assert_eq!(
        ty,
        TypeInfo::ArrayShape(vec![ArrayShapeItem {
            key: Some("self".into()),
            optional: true,
            value: TypeInfo::Nullable(Box::new(TypeInfo::Self_))
        }])
    );
}

#[test]
fn transform_rejects_partial_failure_and_excessive_depth_or_width() {
    let ty = TypeInfo::Union(vec![TypeInfo::Self_, TypeInfo::Parent_]);
    assert!(
        try_transform_type(&ty, &mut |node| if node == TypeInfo::Parent_ {
            None
        } else {
            Some(node)
        })
        .is_none()
    );
    let mut deep = TypeInfo::Self_;
    for _ in 0..=MAX_DEPTH {
        deep = TypeInfo::Nullable(Box::new(deep));
    }
    assert!(try_transform_type(&deep, &mut Some).is_none());
    let wide = TypeInfo::Union(vec![TypeInfo::Self_; MAX_MATCH_STEPS + 1]);
    let mut visited = 0;
    assert!(try_transform_type(&wide, &mut |node| {
        visited += 1;
        Some(node)
    })
    .is_none());
    assert_eq!(visited, 0);
}
