mod support;

use support::*;

#[tokio::test(flavor = "current_thread")]
async fn test_call_hierarchy_prepare_incoming_and_outgoing() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request(1))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let code = r#"<?php
namespace App;

function helper(): void {}

function caller(): void {
    helper();
}

class Service {
    public function target(): void {}

    public function entry(): void {
        $this->target();
        helper();
    }
}

function run(Service $service): void {
    caller();
    $service->entry();
}
"#;
    let uri = "file:///test/call-hierarchy.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let entry_prepare = service
        .ready()
        .await
        .unwrap()
        .call(prepare_call_hierarchy_request(2, uri, 12, 22))
        .await
        .unwrap();
    let entry_result = extract_result(entry_prepare);
    let entry_items = entry_result
        .as_array()
        .expect("expected prepareCallHierarchy item array");
    let entry_item = entry_items[0].clone();
    assert_eq!(entry_item["name"].as_str(), Some("entry"));
    assert_eq!(
        entry_item["data"]["fqn"].as_str(),
        Some("App\\Service::entry"),
        "expected call hierarchy item data, got: {}",
        entry_item
    );

    let outgoing_resp = service
        .ready()
        .await
        .unwrap()
        .call(outgoing_calls_request(3, entry_item))
        .await
        .unwrap();
    let outgoing_result = extract_result(outgoing_resp);
    let outgoing = outgoing_result
        .as_array()
        .expect("expected outgoing call array");
    let outgoing_names: Vec<&str> = outgoing
        .iter()
        .filter_map(|call| call["to"]["name"].as_str())
        .collect();
    assert!(
        outgoing_names.contains(&"target") && outgoing_names.contains(&"helper"),
        "expected outgoing target/helper calls, got: {}",
        outgoing_result
    );
    assert!(
        outgoing.iter().any(|call| call["fromRanges"]
            .as_array()
            .is_some_and(|ranges| !ranges.is_empty())),
        "expected outgoing call ranges, got: {}",
        outgoing_result
    );

    let helper_prepare = service
        .ready()
        .await
        .unwrap()
        .call(prepare_call_hierarchy_request(4, uri, 3, 10))
        .await
        .unwrap();
    let helper_result = extract_result(helper_prepare);
    let helper_item = helper_result
        .as_array()
        .expect("expected helper prepare item array")[0]
        .clone();
    assert_eq!(helper_item["name"].as_str(), Some("helper"));

    let incoming_resp = service
        .ready()
        .await
        .unwrap()
        .call(incoming_calls_request(5, helper_item))
        .await
        .unwrap();
    let incoming_result = extract_result(incoming_resp);
    let incoming = incoming_result
        .as_array()
        .expect("expected incoming call array");
    let incoming_names: Vec<&str> = incoming
        .iter()
        .filter_map(|call| call["from"]["name"].as_str())
        .collect();
    assert!(
        incoming_names.contains(&"caller") && incoming_names.contains(&"entry"),
        "expected incoming caller/entry calls, got: {}",
        incoming_result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_type_hierarchy_prepare_supertypes_and_subtypes() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request(1))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let code = r#"<?php
namespace App;

interface Contract {}

class Base {}

class Child extends Base implements Contract {}

class GrandChild extends Child {}

class Other implements Contract {}
"#;
    let uri = "file:///test/type-hierarchy.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let child_prepare = service
        .ready()
        .await
        .unwrap()
        .call(prepare_type_hierarchy_request(2, uri, 7, 8))
        .await
        .unwrap();
    let child_result = extract_result(child_prepare);
    let child_item = child_result
        .as_array()
        .expect("expected type hierarchy prepare array")[0]
        .clone();
    assert_eq!(child_item["name"].as_str(), Some("Child"));
    assert_eq!(child_item["data"]["fqn"].as_str(), Some("App\\Child"));

    let supertypes_resp = service
        .ready()
        .await
        .unwrap()
        .call(type_hierarchy_supertypes_request(3, child_item.clone()))
        .await
        .unwrap();
    let supertypes_result = extract_result(supertypes_resp);
    let supertypes = supertypes_result
        .as_array()
        .expect("expected supertypes array");
    let supertype_names: Vec<&str> = supertypes
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect();
    assert!(
        supertype_names.contains(&"Base") && supertype_names.contains(&"Contract"),
        "expected Base and Contract supertypes, got: {}",
        supertypes_result
    );

    let child_subtypes_resp = service
        .ready()
        .await
        .unwrap()
        .call(type_hierarchy_subtypes_request(4, child_item))
        .await
        .unwrap();
    let child_subtypes_result = extract_result(child_subtypes_resp);
    let child_subtype_names: Vec<&str> = child_subtypes_result
        .as_array()
        .expect("expected child subtypes array")
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect();
    assert_eq!(child_subtype_names, vec!["GrandChild"]);

    let contract_prepare = service
        .ready()
        .await
        .unwrap()
        .call(prepare_type_hierarchy_request(5, uri, 3, 12))
        .await
        .unwrap();
    let contract_result = extract_result(contract_prepare);
    let contract_item = contract_result
        .as_array()
        .expect("expected contract prepare array")[0]
        .clone();
    assert_eq!(contract_item["name"].as_str(), Some("Contract"));

    let contract_subtypes_resp = service
        .ready()
        .await
        .unwrap()
        .call(type_hierarchy_subtypes_request(6, contract_item))
        .await
        .unwrap();
    let contract_subtypes_result = extract_result(contract_subtypes_resp);
    let contract_subtype_names: Vec<&str> = contract_subtypes_result
        .as_array()
        .expect("expected contract subtypes array")
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect();
    assert!(
        contract_subtype_names.contains(&"Child") && contract_subtype_names.contains(&"Other"),
        "expected Child and Other contract subtypes, got: {}",
        contract_subtypes_result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}
