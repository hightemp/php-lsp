use super::*;

fn parser(source: &str) -> FileParser {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    parser
}
fn assert_chains(source: &str, positions: &[Position], ranges: &[SelectionRange]) {
    let coordinates = SourcePositions::new(source);
    assert_eq!(positions.len(), ranges.len());
    for (position, chain) in positions.iter().zip(ranges) {
        let (point, _) = coordinates.normalize(*position).unwrap();
        let mut current = Some(chain);
        let mut child: Option<Range> = None;
        let mut count = 0;
        while let Some(item) = current {
            count += 1;
            let range = item.range;
            assert!(
                range.start <= point && point <= range.end,
                "{position:?}: {range:?}"
            );
            assert_eq!(coordinates.normalize(range.start).unwrap().0, range.start);
            assert_eq!(coordinates.normalize(range.end).unwrap().0, range.end);
            if let Some(child) = child {
                assert!(range != child && range.start <= child.start && child.end <= range.end);
            }
            child = Some(range);
            current = item.parent.as_deref();
        }
        assert!(count <= MAX_SELECTION_RANGES);
    }
}

#[test]
fn p239_template_selection_boundaries_repeated_projections_and_generated_prelude() {
    for document in [
        preprocess_blade_template("😀 @{{ untouched }}\r\n{{ $user->name }} {!! $other !!}\r\n{{-- comment --}}"),
        preprocess_twig_template("😀 {{ user.name }} {{ user.name|upper }}\r\n{% for k, item in user.items %}{{ item }}{% endfor %}\r\n",&[TemplateVariableType {name:"user".into(),type_text:"App\\User".into(),shape_definitions:vec![]}]),
    ] {
        let source=document.original_source();
        let coordinates=SourcePositions::new(source);
        let positions=source.char_indices().map(|(offset,_)| coordinates.position(offset)).chain(std::iter::once(coordinates.position(source.len()))).collect::<Vec<_>>();
        let parser=parser(document.virtual_source());
        let ranges=selection_ranges(parser.tree().unwrap(),document.virtual_source(),Some(&document),&positions).unwrap();
        assert_chains(source,&positions,&ranges);
        // The virtual prelude is longer than the original document; it cannot
        // make an out-of-document input line valid.
        assert!(selection_ranges(parser.tree().unwrap(),document.virtual_source(),Some(&document),&[positions[0],Position::new(source.split('\n').count() as u32,0)]).is_err());
    }
}

#[test]
fn p239_selection_keeps_one_parser_template_snapshot_during_edit_and_reopen() {
    for (reopen, ending) in [false, true]
        .into_iter()
        .flat_map(|reopen| ["\n", "\r\n", "\r"].map(|ending| (reopen, ending)))
    {
        let uri = "file:///p239/race.twig";
        let before = preprocess_twig_template(&format!("{{{{ first.name }}}}{ending}"), &[]);
        let after = preprocess_twig_template(
            &format!("plain text{ending}{{{{ second.email }}}}{ending}"),
            &[],
        );
        let positions = [Position::new(0, 5), Position::new(1, 0)];
        let open = Arc::new(DashMap::new());
        let templates = Arc::new(DashMap::new());
        open.insert(uri.into(), parser(before.virtual_source()));
        templates.insert(uri.into(), before.clone());
        let expected =
            selection_ranges_for_open_document(&open, &templates, uri, &positions, || {})
                .unwrap()
                .unwrap();
        let (reached, wait) = std::sync::mpsc::channel();
        let (resume, release) = std::sync::mpsc::channel();
        let reader_open = open.clone();
        let reader_templates = templates.clone();
        let reader = std::thread::spawn(move || {
            selection_ranges_for_open_document(
                &reader_open,
                &reader_templates,
                uri,
                &positions,
                || {
                    reached.send(()).unwrap();
                    release.recv_timeout(Duration::from_secs(5)).unwrap();
                },
            )
            .unwrap()
            .unwrap()
        });
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            open.try_get_mut(uri),
            dashmap::try_result::TryResult::Locked
        ));
        let writer_open = open.clone();
        let writer_templates = templates.clone();
        let (started, waiting) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            started.send(()).unwrap();
            if reopen {
                writer_open.remove(uri);
                writer_templates.remove(uri);
                let entry = writer_open.entry(uri.into());
                writer_templates.insert(uri.into(), after.clone());
                entry.insert(parser(after.virtual_source()));
            } else {
                let mut entry = writer_open.get_mut(uri).unwrap();
                writer_templates.insert(uri.into(), after.clone());
                *entry = parser(after.virtual_source());
            }
        });
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        resume.send(()).unwrap();
        let old = reader.join().unwrap();
        writer.join().unwrap();
        assert_eq!(
            old, expected,
            "mixed original and virtual source during reopen={reopen}"
        );
        assert_chains(before.original_source(), &positions, &old);
        let new = selection_ranges_for_open_document(&open, &templates, uri, &positions, || {})
            .unwrap()
            .unwrap();
        assert_ne!(old, new);
        assert_chains(
            templates.get(uri).unwrap().original_source(),
            &positions,
            &new,
        );
    }
}

#[test]
fn line_model_selection_reader_serializes_two_cr_edits_with_the_parser_snapshot() {
    let uri = "file:///line-model-race.php";
    let before = "<?php\rfunction first() {}\r";
    let after = "<?php\r// changed\rfunction other() {}\r";
    let open = Arc::new(DashMap::new());
    let templates = Arc::new(DashMap::new());
    open.insert(uri.into(), parser(before));
    let positions = [Position::new(1, 10)];
    let expected = selection_ranges_for_open_document(&open, &templates, uri, &positions, || {})
        .unwrap()
        .unwrap();
    let (entered, wait) = std::sync::mpsc::channel();
    let (resume, release) = std::sync::mpsc::channel();
    let reader_open = open.clone();
    let reader_templates = templates.clone();
    let reader = std::thread::spawn(move || {
        selection_ranges_for_open_document(&reader_open, &reader_templates, uri, &positions, || {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(5)).unwrap();
        })
        .unwrap()
        .unwrap()
    });
    wait.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        open.try_get_mut(uri),
        dashmap::try_result::TryResult::Locked
    ));
    let writer_open = open.clone();
    let (started, waiting) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        started.send(()).unwrap();
        let mut parser = writer_open.get_mut(uri).unwrap();
        parser.apply_edit(1, 0, 1, 0, "// changed\r").unwrap();
        parser.apply_edit(2, 9, 2, 14, "other").unwrap();
        assert_eq!(parser.source(), after);
    });
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    resume.send(()).unwrap();
    assert_eq!(reader.join().unwrap(), expected);
    writer.join().unwrap();
    let actual =
        selection_ranges_for_open_document(&open, &templates, uri, &[Position::new(2, 10)], || {})
            .unwrap()
            .unwrap();
    assert_eq!(
        actual[0].range,
        Range::new(Position::new(2, 9), Position::new(2, 14))
    );
    assert_chains(after, &[Position::new(2, 10)], &actual);
}

#[test]
fn p239_selection_missing_document_or_tree_and_empty_batch_are_explicit() {
    let open = DashMap::new();
    let templates = DashMap::new();
    let uri = "file:///p239/missing.php";
    assert!(selection_ranges_for_open_document(
        &open,
        &templates,
        uri,
        &[Position::new(0, 0)],
        || {}
    )
    .unwrap()
    .is_none());
    assert_eq!(
        selection_ranges_for_open_document(&open, &templates, uri, &[], || {}).unwrap(),
        Some(vec![])
    );
    open.insert(uri.into(), FileParser::new());
    assert!(selection_ranges_for_open_document(
        &open,
        &templates,
        uri,
        &[Position::new(0, 0)],
        || {}
    )
    .unwrap()
    .is_none());
}
