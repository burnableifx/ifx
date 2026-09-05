//! `linode.*` handlers against a wiremock stand-in for the Linode v4 API.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ifx::engine::{Engine, Options};
use ifx::model::{OutputRef, Program, ResourceDecl, Urn};
use ifx::provider::{Actual, Ctx, Handler, Registry};
use ifx::providers::linode::{
    self, DomainHandler, DomainRecordHandler, FirewallHandler, InstanceHandler, Linode,
};
use ifx::state::State;
use ifx::transport::TransportPool;
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Responds with each template in turn, then keeps returning the last one.
struct Sequence {
    responses: Vec<ResponseTemplate>,
    n: AtomicUsize,
}

impl Sequence {
    fn new(responses: Vec<ResponseTemplate>) -> Self {
        Self {
            responses,
            n: AtomicUsize::new(0),
        }
    }
}

impl Respond for Sequence {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let i = self.n.fetch_add(1, Ordering::SeqCst);
        self.responses[i.min(self.responses.len() - 1)].clone()
    }
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

fn not_found() -> ResponseTemplate {
    ResponseTemplate::new(404).set_body_json(json!({"errors": [{"reason": "Not found"}]}))
}

fn page(items: Vec<Value>) -> Value {
    json!({"data": items, "page": 1, "pages": 1, "results": items.len()})
}

fn instance(id: i64, status: &str, ty: &str) -> Value {
    json!({
        "id": id, "label": "web", "region": "us-east", "type": ty, "image": "linode/debian12",
        "status": status, "ipv4": ["203.0.113.10", "192.168.1.5"], "ipv6": "2600:3c03::f03c:91ff:fe24:3a2f/128",
        "tags": ["prod"], "backups": {"enabled": false},
    })
}

fn firewall(id: i64) -> Value {
    json!({
        "id": id, "label": "fw", "status": "enabled", "tags": [],
        "rules": {
            "inbound_policy": "DROP", "outbound_policy": "ACCEPT",
            "inbound": [{"label": "ssh", "action": "ACCEPT", "protocol": "TCP", "ports": "22",
                         "addresses": {"ipv4": ["0.0.0.0/0"], "ipv6": null}, "description": null}],
            "outbound": [],
        },
    })
}

fn ssh_rule() -> Value {
    json!({"label": "ssh", "action": "ACCEPT", "protocol": "TCP", "ports": "22",
           "addresses": {"ipv4": ["0.0.0.0/0"]}})
}

async fn server() -> (MockServer, Arc<Linode>) {
    let server = MockServer::start().await;
    let api = Linode::with_base_url(server.uri(), "test-token")
        .with_poll_interval(Duration::from_millis(1))
        .with_wait_timeout(Duration::from_secs(5))
        .with_retry_backoff(Duration::from_millis(1));
    (server, Arc::new(api))
}

/// Every instance read also asks which firewall is attached.
async fn mount_instance_firewalls(server: &MockServer, id: i64, firewalls: Vec<Value>) {
    Mock::given(method("GET"))
        .and(path(format!("/linode/instances/{id}/firewalls")))
        .respond_with(ok(page(firewalls)))
        .mount(server)
        .await;
}

fn cx<'a>(urn: &'a Urn, pool: &'a TransportPool) -> Ctx<'a> {
    Ctx {
        urn,
        transports: pool,
        triggered: false,
    }
}

fn web_inputs() -> Value {
    json!({
        "label": "web", "region": "us-east", "type": "g6-nanode-1", "image": "linode/debian12",
        "authorized_keys": ["ssh-ed25519 AAAA test"], "tags": ["prod"], "booted": true,
        "user_data": "#cloud-config\npackages: [nginx]\n", "ssh_user": "root",
        "connect_timeout_secs": 120,
    })
}

#[tokio::test]
async fn create_instance_polls_until_running() {
    let (server, api) = server().await;
    let mut inputs = web_inputs();
    inputs["disk_encryption"] = json!("enabled");
    inputs["swap_size"] = json!(1024);
    inputs["interface_generation"] = json!("linode");
    inputs["network_helper"] = json!(true);
    inputs["placement_group_id"] = json!(12);
    inputs["maintenance_policy"] = json!("linode/migrate");
    inputs["watchdog_enabled"] = json!(true);
    inputs["alerts"] = json!({"cpu": 180});
    inputs["backups_enabled"] = json!(true);
    inputs["backup_schedule"] = json!({"day": "Saturday", "window": "W22"});
    Mock::given(method("POST"))
        .and(path("/linode/instances"))
        .and(header("authorization", "Bearer test-token"))
        .and(body_partial_json(json!({
            "label": "web", "region": "us-east", "type": "g6-nanode-1", "booted": true,
            "authorized_keys": ["ssh-ed25519 AAAA test"],
            "disk_encryption": "enabled", "swap_size": 1024,
            "interface_generation": "linode", "network_helper": true,
            "placement_group": {"id": 12},
            "backups_enabled": true,
            "metadata": {"user_data": "I2Nsb3VkLWNvbmZpZwpwYWNrYWdlczogW25naW54XQo="},
        })))
        .respond_with(ok(instance(42, "provisioning", "g6-nanode-1")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/linode/instances/42"))
        .respond_with(Sequence::new(vec![
            ok(instance(42, "provisioning", "g6-nanode-1")),
            ok(instance(42, "booting", "g6-nanode-1")),
            ok(instance(42, "running", "g6-nanode-1")),
        ]))
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/linode/instances/42"))
        .and(body_partial_json(json!({
            "alerts": {"cpu": 180}, "maintenance_policy": "linode/migrate",
            "watchdog_enabled": true,
            "backups": {"schedule": {"day": "Saturday", "window": "W22"}},
        })))
        .respond_with(ok(instance(42, "running", "g6-nanode-1")))
        .expect(1)
        .mount(&server)
        .await;

    let h = InstanceHandler::new(api);
    let urn = Urn::new("linode.instance", "web");
    let pool = TransportPool::new();
    let applied = h.create(&cx(&urn, &pool), &inputs).await.unwrap();
    assert_eq!(applied.id.as_deref(), Some("42"));
    assert_eq!(applied.outputs["ipv4"], "203.0.113.10");
    assert_eq!(
        applied.outputs["ipv4s"],
        json!(["203.0.113.10", "192.168.1.5"])
    );
    assert_eq!(applied.outputs["ipv6"], "2600:3c03::f03c:91ff:fe24:3a2f");
    assert_eq!(applied.outputs["status"], "running");
    assert_eq!(
        applied.outputs["connection"],
        json!({"kind": "ssh", "host": "203.0.113.10", "user": "root", "port": null, "identity": null,
               "connect_timeout_secs": 120, "sudo": false, "extra_args": []})
    );
    let conn: ifx::Connection =
        serde_json::from_value(applied.outputs["connection"].clone()).unwrap();
    assert_eq!(conn.label(), "ssh://root@203.0.113.10");
}

#[tokio::test]
async fn backup_schedule_requires_enrollment() {
    let (_server, api) = server().await;
    let mut inputs = web_inputs();
    inputs["backup_schedule"] = json!({"day": "Saturday"});
    let handler = InstanceHandler::new(api);
    let urn = Urn::new("linode.instance", "web");
    let pool = TransportPool::new();

    let error = handler.create(&cx(&urn, &pool), &inputs).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "`backup_schedule` requires `backups_enabled` to be true"
    );
}

#[tokio::test]
async fn image_provisioning_requires_authentication() {
    let (_server, api) = server().await;
    let inputs = json!({
        "label": "web",
        "region": "us-east",
        "type": "g6-nanode-1",
        "image": "linode/debian12",
    });
    let handler = InstanceHandler::new(api);
    let urn = Urn::new("linode.instance", "web");
    let pool = TransportPool::new();

    let error = handler.create(&cx(&urn, &pool), &inputs).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "image provisioning requires `root_pass`, `authorized_keys`, or `authorized_users`"
    );
}

#[tokio::test]
async fn empty_and_stackscript_sources_are_validated() {
    let (_server, api) = server().await;
    let handler = InstanceHandler::new(api);
    let urn = Urn::new("linode.instance", "web");
    let pool = TransportPool::new();
    let base = json!({"label": "web", "region": "us-east", "type": "g6-nanode-1"});

    let error = handler.create(&cx(&urn, &pool), &base).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "empty Linode provisioning requires `booted` to be false"
    );

    let mut stackscript = base;
    stackscript["booted"] = json!(false);
    stackscript["stackscript_id"] = json!(123);
    let error = handler
        .create(&cx(&urn, &pool), &stackscript)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "`stackscript_id` requires an `image` source"
    );
}

#[tokio::test]
async fn read_by_id_reports_props_and_missing_as_none() {
    let (server, api) = server().await;
    Mock::given(method("GET"))
        .and(path("/linode/instances/42"))
        .respond_with(ok(instance(42, "running", "g6-nanode-1")))
        .mount(&server)
        .await;
    mount_instance_firewalls(&server, 42, vec![json!({"id": 9, "label": "fw"})]).await;
    Mock::given(method("GET"))
        .and(path("/linode/instances/7"))
        .respond_with(not_found())
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/linode/instances"))
        .and(header("X-Filter", r#"{"label":"gone"}"#))
        .respond_with(ok(page(vec![])))
        .mount(&server)
        .await;

    let h = InstanceHandler::new(api);
    let urn = Urn::new("linode.instance", "web");
    let pool = TransportPool::new();
    let actual = h
        .read(&cx(&urn, &pool), Some("42"), &web_inputs())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual.id.as_deref(), Some("42"));
    assert_eq!(
        actual.props,
        json!({
            "label": "web", "region": "us-east", "type": "g6-nanode-1", "image": "linode/debian12",
            "backup_id": null, "root_pass": null,
            "authorized_keys": ["ssh-ed25519 AAAA test"], "authorized_users": null,
            "disk_encryption": null, "boot_size": null, "swap_size": null,
            "interface_generation": null, "interfaces": null, "ipv4": null, "kernel": null,
            "network_helper": null,
            "placement_group_id": null, "maintenance_policy": null, "watchdog_enabled": null,
            "stackscript_id": null, "stackscript_data": null,
            "alerts": null,
            "tags": ["prod"], "private_ip": true, "backups_enabled": false, "booted": true,
            "backup_schedule": null,
            "firewall_id": 9, "user_data": "#cloud-config\npackages: [nginx]\n",
            "ssh_user": "root", "connect_timeout_secs": 120,
        })
    );
    let diff = h.diff(&web_inputs(), &actual).unwrap();
    assert!(diff.is_empty(), "{diff:?}");

    let mut changed = web_inputs();
    changed["authorized_keys"] = json!(["ssh-ed25519 BBBB replacement"]);
    changed["user_data"] = json!("#cloud-config\nruncmd: [reboot]\n");
    let diff = h.diff(&changed, &actual).unwrap();
    assert!(diff.requires_replace());
    let fields: Vec<&str> = diff
        .changes
        .iter()
        .map(|change| change.field.as_str())
        .collect();
    assert_eq!(fields, ["authorized_keys", "user_data"]);

    let gone = h
        .read(&cx(&urn, &pool), Some("7"), &json!({"label": "gone"}))
        .await
        .unwrap();
    assert!(gone.is_none());
}

#[tokio::test]
async fn instance_nested_settings_compare_only_managed_fields() {
    let (server, api) = server().await;
    let mut response = instance(42, "running", "g6-nanode-1");
    response["alerts"] = json!({"cpu": 180, "io": 10_000, "network_in": 10});
    response["backups"]["schedule"] = json!({"day": "Saturday", "window": "W22"});
    Mock::given(method("GET"))
        .and(path("/linode/instances/42"))
        .respond_with(ok(response))
        .mount(&server)
        .await;
    mount_instance_firewalls(&server, 42, vec![]).await;
    let mut inputs = web_inputs();
    inputs["alerts"] = json!({"cpu": 180});
    inputs["backup_schedule"] = json!({"day": "Saturday"});

    let handler = InstanceHandler::new(api);
    let urn = Urn::new("linode.instance", "web");
    let pool = TransportPool::new();
    let actual = handler
        .read(&cx(&urn, &pool), Some("42"), &inputs)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual.props["alerts"], json!({"cpu": 180}));
    assert_eq!(actual.props["backup_schedule"], json!({"day": "Saturday"}));
    assert!(handler.diff(&inputs, &actual).unwrap().is_empty());
}

#[tokio::test]
async fn adopts_existing_instance_by_label() {
    let (server, api) = server().await;
    Mock::given(method("GET"))
        .and(path("/linode/instances"))
        .and(header("X-Filter", r#"{"label":"web"}"#))
        .and(query_param("page", "1"))
        .respond_with(ok(page(vec![instance(42, "running", "g6-nanode-1")])))
        .expect(1)
        .mount(&server)
        .await;
    mount_instance_firewalls(&server, 42, vec![]).await;

    let h = InstanceHandler::new(api);
    let urn = Urn::new("linode.instance", "web");
    let pool = TransportPool::new();
    let actual = h
        .read(&cx(&urn, &pool), None, &web_inputs())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual.id.as_deref(), Some("42"));
    assert_eq!(actual.props["firewall_id"], Value::Null);
    assert_eq!(actual.outputs["id"], 42);
}

#[tokio::test]
async fn update_tags_puts_and_resize_on_type_change() {
    let (server, api) = server().await;
    Mock::given(method("PUT"))
        .and(path("/linode/instances/42"))
        .and(body_partial_json(json!({"tags": ["prod", "web"]})))
        .respond_with(ok(instance(42, "running", "g6-nanode-1")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/linode/instances/42"))
        .respond_with(ok(instance(42, "running", "g6-nanode-1")))
        .mount(&server)
        .await;
    let h = InstanceHandler::new(api.clone());
    let urn = Urn::new("linode.instance", "web");
    let pool = TransportPool::new();
    let mut actual = Actual {
        id: Some("42".into()),
        props: json!({"label": "web", "region": "us-east", "type": "g6-nanode-1", "tags": ["prod"],
                      "booted": true, "backups_enabled": false}),
        outputs: json!({}),
    };
    let mut desired = web_inputs();
    desired["tags"] = json!(["web", "prod"]);
    let diff = h.diff(&desired, &actual).unwrap();
    assert_eq!(diff.changes.len(), 1);
    assert_eq!(diff.changes[0].field, "tags");
    assert!(!diff.requires_replace());
    desired["tags"] = json!(["prod", "web"]);
    h.update(&cx(&urn, &pool), Some("42"), &desired, &actual)
        .await
        .unwrap();

    // Resize: POST /resize, then poll until the new type is reported and status settles.
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/linode/instances/42/resize"))
        .and(body_partial_json(
            json!({"type": "g6-standard-2", "allow_auto_disk_resize": true}),
        ))
        .respond_with(ok(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    let mut resized = instance(42, "running", "g6-standard-2");
    resized["tags"] = json!(["prod", "web"]);
    Mock::given(method("GET"))
        .and(path("/linode/instances/42"))
        .respond_with(Sequence::new(vec![
            ok(instance(42, "resizing", "g6-nanode-1")),
            ok(instance(42, "resizing", "g6-standard-2")),
            ok(resized),
        ]))
        .mount(&server)
        .await;
    actual.props["tags"] = json!(["prod", "web"]);
    desired["type"] = json!("g6-standard-2");
    let diff = h.diff(&desired, &actual).unwrap();
    assert_eq!(diff.changes.len(), 1);
    assert_eq!(diff.changes[0].field, "type");
    let applied = h
        .update(&cx(&urn, &pool), Some("42"), &desired, &actual)
        .await
        .unwrap();
    assert_eq!(applied.outputs["type"], "g6-standard-2");
    assert_eq!(applied.outputs["status"], "running");
}

#[tokio::test]
async fn replace_fields_force_replacement() {
    let (_server, api) = server().await;
    let h = InstanceHandler::new(api);
    let actual = Actual {
        id: Some("42".into()),
        props: json!({"label": "web", "region": "us-east", "type": "g6-nanode-1", "image": "linode/debian12"}),
        outputs: json!({}),
    };
    let mut desired = web_inputs();
    desired["region"] = json!("eu-central");
    let diff = h.diff(&desired, &actual).unwrap();
    assert!(diff.requires_replace());
    assert_eq!(diff.changes[0].field, "region");
}

#[tokio::test]
async fn delete_then_confirms_404() {
    let (server, api) = server().await;
    Mock::given(method("DELETE"))
        .and(path("/linode/instances/42"))
        .respond_with(ok(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/linode/instances/42"))
        .respond_with(Sequence::new(vec![
            ok(instance(42, "shutting_down", "g6-nanode-1")),
            not_found(),
        ]))
        .expect(2)
        .mount(&server)
        .await;
    let h = InstanceHandler::new(api);
    let urn = Urn::new("linode.instance", "web");
    let pool = TransportPool::new();
    h.delete(&cx(&urn, &pool), Some("42"), &web_inputs())
        .await
        .unwrap();
}

#[tokio::test]
async fn firewall_create_then_update_rules_and_devices() {
    let (server, api) = server().await;
    let h = FirewallHandler::new(api);
    let urn = Urn::new("linode.firewall", "fw");
    let pool = TransportPool::new();
    let inputs = json!({
        "label": "fw", "inbound_policy": "DROP", "outbound_policy": "ACCEPT",
        "inbound": [ssh_rule()], "linodes": [42], "interfaces": [77], "nodebalancers": [88],
    });

    Mock::given(method("POST"))
        .and(path("/networking/firewalls"))
        .and(body_partial_json(json!({
            "label": "fw",
            "rules": {"inbound_policy": "DROP", "outbound_policy": "ACCEPT", "inbound": [ssh_rule()], "outbound": []},
            "devices": {"linodes": [42], "interfaces": [77], "nodebalancers": [88]},
        })))
        .respond_with(ok(firewall(9)))
        .expect(1)
        .mount(&server)
        .await;
    let applied = h.create(&cx(&urn, &pool), &inputs).await.unwrap();
    assert_eq!(applied.id.as_deref(), Some("9"));
    assert_eq!(applied.outputs, json!({"id": 9, "status": "enabled"}));

    // Read normalises API rules (nulls dropped) so the desired rules diff clean.
    Mock::given(method("GET"))
        .and(path("/networking/firewalls/9"))
        .respond_with(ok(firewall(9)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/networking/firewalls/9/devices"))
        .respond_with(ok(page(vec![
            json!({"id": 501, "entity": {"id": 42, "type": "linode"}}),
            json!({"id": 502, "entity": {"id": 77, "type": "interface"}}),
            json!({"id": 503, "entity": {"id": 88, "type": "nodebalancer"}}),
        ])))
        .mount(&server)
        .await;
    let actual = h
        .read(&cx(&urn, &pool), Some("9"), &inputs)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual.props["inbound"], json!([ssh_rule()]));
    assert_eq!(actual.props["linodes"], json!([42]));
    assert_eq!(actual.props["interfaces"], json!([77]));
    assert_eq!(actual.props["nodebalancers"], json!([88]));
    assert!(h.diff(&inputs, &actual).unwrap().is_empty());

    // Add a rule and swap the attached instance.
    let mut desired = inputs.clone();
    let http = json!({"label": "http", "action": "ACCEPT", "protocol": "TCP", "ports": "80,443",
                      "addresses": {"ipv4": ["0.0.0.0/0"], "ipv6": ["::/0"]}});
    desired["inbound"] = json!([ssh_rule(), http]);
    desired["linodes"] = json!([43]);
    desired["interfaces"] = json!([78]);
    let diff = h.diff(&desired, &actual).unwrap();
    let fields: Vec<&str> = diff.changes.iter().map(|c| c.field.as_str()).collect();
    assert_eq!(fields, ["inbound", "linodes", "interfaces"]);

    Mock::given(method("PUT"))
        .and(path("/networking/firewalls/9/rules"))
        .and(body_partial_json(
            json!({"inbound": [ssh_rule(), http], "inbound_policy": "DROP",
                                      "outbound": [], "outbound_policy": "ACCEPT"}),
        ))
        .respond_with(ok(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/networking/firewalls/9/devices/501"))
        .respond_with(ok(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/networking/firewalls/9/devices"))
        .and(body_partial_json(json!({"id": 43, "type": "linode"})))
        .respond_with(ok(json!({"id": 502})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/networking/firewalls/9/devices/502"))
        .respond_with(ok(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/networking/firewalls/9/devices"))
        .and(body_partial_json(
            json!({"id": 78, "type": "linode_interface"}),
        ))
        .respond_with(ok(json!({"id": 504})))
        .expect(1)
        .mount(&server)
        .await;
    let applied = h
        .update(&cx(&urn, &pool), Some("9"), &desired, &actual)
        .await
        .unwrap();
    assert_eq!(applied.outputs["id"], 9);
}

#[tokio::test]
async fn domain_and_record_adopt_by_identity() {
    let (server, api) = server().await;
    Mock::given(method("GET"))
        .and(path("/domains"))
        .and(header("X-Filter", r#"{"domain":"example.com"}"#))
        .respond_with(ok(page(vec![json!({
            "id": 7, "domain": "example.com", "type": "master", "soa_email": "ops@example.com",
            "ttl_sec": 300, "tags": [],
        })])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/domains/7/records"))
        .respond_with(ok(page(vec![
            json!({"id": 100, "type": "A", "name": "www", "target": "203.0.113.10", "ttl_sec": 0, "priority": 0}),
            json!({"id": 101, "type": "AAAA", "name": "www", "target": "2600::1", "ttl_sec": 0, "priority": 0}),
        ])))
        .mount(&server)
        .await;

    let pool = TransportPool::new();
    let dh = DomainHandler::new(api.clone());
    let durn = Urn::new("linode.domain", "zone");
    let dom = json!({"domain": "example.com", "type": "master", "soa_email": "ops@example.com", "ttl_sec": 300});
    let actual = dh
        .read(&cx(&durn, &pool), None, &dom)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual.id.as_deref(), Some("7"));
    assert_eq!(actual.outputs, json!({"id": 7}));
    assert!(dh.diff(&dom, &actual).unwrap().is_empty());
    let mut renamed = dom.clone();
    renamed["domain"] = json!("example.org");
    assert!(dh.diff(&renamed, &actual).unwrap().requires_replace());

    let rh = DomainRecordHandler::new(api.clone());
    let rurn = Urn::new("linode.domain_record", "www6");
    let rec = json!({"domain_id": 7, "type": "AAAA", "name": "www", "target": "2600::1"});
    let actual = rh
        .read(&cx(&rurn, &pool), None, &rec)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual.id.as_deref(), Some("101"));
    assert_eq!(actual.props["target"], "2600::1");
    assert!(rh.diff(&rec, &actual).unwrap().is_empty());
    let missing = json!({"domain_id": 7, "type": "TXT", "name": "www", "target": "x"});
    assert!(
        rh.read(&cx(&rurn, &pool), None, &missing)
            .await
            .unwrap()
            .is_none()
    );

    // Master domains need an SOA email before we even call the API.
    let err = dh
        .create(
            &cx(&durn, &pool),
            &json!({"domain": "x.com", "type": "master"}),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("soa_email"), "{err}");
}

#[tokio::test]
async fn retries_429_honouring_retry_after() {
    let (server, api) = server().await;
    Mock::given(method("GET"))
        .and(path("/linode/instances/42"))
        .respond_with(Sequence::new(vec![
            ResponseTemplate::new(429).insert_header("Retry-After", "0"),
            ResponseTemplate::new(429),
            ok(instance(42, "running", "g6-nanode-1")),
        ]))
        .expect(3)
        .mount(&server)
        .await;
    let v = api.get("/linode/instances/42").await.unwrap();
    assert_eq!(v["id"], 42);
}

#[tokio::test]
async fn surfaces_error_body_fields() {
    let (server, api) = server().await;
    Mock::given(method("POST"))
        .and(path("/linode/instances"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "errors": [{"reason": "region is not valid", "field": "region"},
                       {"reason": "Label must be unique", "field": "label"}]
        })))
        .mount(&server)
        .await;
    let h = InstanceHandler::new(api);
    let urn = Urn::new("linode.instance", "web");
    let pool = TransportPool::new();
    let err = h.create(&cx(&urn, &pool), &web_inputs()).await.unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("POST /linode/instances: HTTP 400"), "{msg}");
    assert!(msg.contains("region: region is not valid"), "{msg}");
    assert!(msg.contains("label: Label must be unique"), "{msg}");
    let api_err = err.downcast_ref::<linode::ApiError>().unwrap();
    assert!(!api_err.is_not_found());
}

// This test is also the subprocess entry point for the credential-source test.
// A child environment avoids unsafe mutation of the parallel test runner's globals.
#[tokio::test]
async fn credential_request_child() {
    let Ok(expected) = std::env::var("IFX_TEST_CREDENTIAL_RESULT") else {
        return;
    };
    let api = Linode::from_env();
    let result = api.get("/profile").await;
    match expected.as_str() {
        "ok" => assert!(result.is_ok(), "{result:?}"),
        "missing" => assert!(matches!(
            result.unwrap_err().downcast_ref::<linode::ApiError>(),
            Some(linode::ApiError::MissingToken)
        )),
        "file_error" => assert!(matches!(
            result.unwrap_err().downcast_ref::<linode::ApiError>(),
            Some(linode::ApiError::CredentialRead { .. })
        )),
        _ => panic!("unknown credential test case"),
    }
}

#[tokio::test]
async fn requests_use_credentials_without_mutating_the_parent_environment() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/profile"))
        .and(header("authorization", "Bearer fixture-token"))
        .respond_with(ok(json!({})))
        .expect(2)
        .mount(&server)
        .await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("linode-token"), "fixture-token\n").unwrap();
    let missing_directory = directory.path().join("missing");
    for (name, credential_directory, token, expected) in [
        (
            "file wins",
            Some(directory.path()),
            Some("wrong-token"),
            "ok",
        ),
        ("environment fallback", None, Some("fixture-token"), "ok"),
        ("missing token", None, None, "missing"),
        (
            "file fails closed",
            Some(missing_directory.as_path()),
            Some("fixture-token"),
            "file_error",
        ),
    ] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .env_clear()
            .args(["--exact", "credential_request_child", "--nocapture"])
            .env("LINODE_API_URL", server.uri())
            .env("IFX_TEST_CREDENTIAL_RESULT", expected);
        if let Some(path) = credential_directory {
            child.env("CREDENTIALS_DIRECTORY", path);
        }
        if let Some(token) = token {
            child.env("LINODE_TOKEN", token);
        }
        let output = tokio::task::spawn_blocking(move || child.output().unwrap())
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{name}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[tokio::test]
async fn paginates_lists() {
    let (server, api) = server().await;
    Mock::given(method("GET"))
        .and(path("/linode/instances"))
        .and(query_param("page", "1"))
        .respond_with(ok(
            json!({"data": [{"id": 1}], "page": 1, "pages": 2, "results": 2}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/linode/instances"))
        .and(query_param("page", "2"))
        .respond_with(ok(
            json!({"data": [{"id": 2}], "page": 2, "pages": 2, "results": 2}),
        ))
        .mount(&server)
        .await;
    let items = api.list("/linode/instances", None).await.unwrap();
    assert_eq!(items, vec![json!({"id": 1}), json!({"id": 2})]);
}

#[tokio::test]
async fn schemas_document_every_field() {
    let (_server, api) = server().await;
    let mut reg = Registry::new();
    linode::register_with(&mut reg, api);
    let names: Vec<&str> = reg.type_names().collect();
    assert_eq!(
        names,
        [
            "linode.domain",
            "linode.domain_record",
            "linode.firewall",
            "linode.instance"
        ]
    );
    for s in reg.schemas() {
        for f in s.inputs.iter().chain(s.outputs.iter()) {
            assert!(!f.doc.is_empty(), "{}.{} has no doc", s.type_name, f.name);
        }
    }
    let inst = reg.get("linode.instance").unwrap().schema();
    assert!(inst.input_field("root_pass").unwrap().sensitive);
    assert!(inst.input_field("user_data").unwrap().sensitive);
    assert!(inst.input_field("stackscript_data").unwrap().sensitive);
    assert!(inst.input_field("region").unwrap().replace);
    assert!(!inst.input_field("type").unwrap().replace);
    assert_eq!(
        inst.input_field("ssh_user").unwrap().default,
        Some(json!("root"))
    );
}

#[tokio::test]
async fn engine_plan_apply_then_no_changes() {
    let (server, api) = server().await;
    let web = Urn::new("linode.instance", "web");
    // Plan + apply: label lookup finds nothing, so create; afterwards read by id.
    Mock::given(method("GET"))
        .and(path("/linode/instances"))
        .and(header("X-Filter", r#"{"label":"web"}"#))
        .respond_with(ok(page(vec![])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/linode/instances"))
        .respond_with(ok(instance(42, "provisioning", "g6-nanode-1")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/linode/instances/42"))
        .respond_with(Sequence::new(vec![
            ok(instance(42, "provisioning", "g6-nanode-1")),
            ok(instance(42, "running", "g6-nanode-1")),
        ]))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/linode/instances/42/firewalls"))
        .respond_with(ok(page(vec![json!({"id": 9})])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/networking/firewalls"))
        .and(header("X-Filter", r#"{"label":"fw"}"#))
        .respond_with(ok(page(vec![])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/networking/firewalls"))
        .and(body_partial_json(json!({"devices": {"linodes": [42]}})))
        .respond_with(ok(firewall(9)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/networking/firewalls/9"))
        .respond_with(ok(firewall(9)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/networking/firewalls/9/devices"))
        .respond_with(ok(page(vec![
            json!({"id": 501, "entity": {"id": 42, "type": "linode"}}),
        ])))
        .mount(&server)
        .await;

    let mut reg = Registry::new();
    linode::register_with(&mut reg, api);
    let engine = Engine::new(reg);
    let mut state = State::default();
    let opts = Options::default();
    let program = Program {
        resources: vec![
            ResourceDecl::new(
                "linode.firewall",
                "fw",
                json!({"label": "fw", "inbound": [ssh_rule()], "linodes": [OutputRef::new(web.clone(), "id").to_value()]}),
            ),
            ResourceDecl::new("linode.instance", "web", web_inputs()),
        ],
    };
    let plan = engine.plan(&program, &state, &opts).await.unwrap();
    assert_eq!(plan.summary().create, 2, "{:?}", plan.ops);
    let report = engine.apply(&program, &mut state, &plan).await.unwrap();
    assert!(report.ok(), "{:?}", report.failed);
    let entry = state.get(&web).unwrap();
    assert_eq!(entry.id.as_deref(), Some("42"));
    assert_eq!(entry.outputs["connection"]["host"], "203.0.113.10");
    let fw = state.get(&Urn::new("linode.firewall", "fw")).unwrap();
    assert_eq!(fw.inputs["linodes"], json!([42]));
    assert_eq!(fw.id.as_deref(), Some("9"));

    let plan = engine.plan(&program, &state, &opts).await.unwrap();
    assert!(!plan.has_changes(), "{:?}", plan.ops);
}

#[test]
fn typed_resources_serialize_like_untyped_inputs() {
    use ifx::providers::linode::{
        DomainRecordType, InstanceRegion, InstanceType, Rule, RuleProtocol,
    };
    use ifx::stack::Stack;
    let mut s = Stack::new();
    let web = s
        .linode_instance("web")
        .label("web")
        .region(InstanceRegion::UsEast)
        .r#type(InstanceType::G6Nanode1)
        .image("linode/debian12")
        .authorized_keys(["ssh-ed25519 AAAA"])
        .add()
        .unwrap();
    s.linode_firewall("fw")
        .label("fw")
        .inbound([Rule::allow("ssh", RuleProtocol::Tcp, "22")])
        .linodes([web.id()])
        .add()
        .unwrap();
    let zone = s
        .linode_domain("zone")
        .domain("example.com")
        .soa_email("ops@example.com")
        .add()
        .unwrap();
    s.linode_domain_record("www")
        .domain_id(zone.id())
        .r#type(DomainRecordType::A)
        .name("www")
        .target(web.ipv4())
        .add()
        .unwrap();
    let conn: ifx::Input<ifx::Connection> = web.connection();
    assert_eq!(
        serde_json::to_value(&conn).unwrap(),
        json!({"$ref": "linode.instance:web", "$path": "connection"})
    );
    let p = s.program();
    assert_eq!(
        p.resources[0].inputs,
        json!({"label": "web", "region": "us-east", "type": "g6-nanode-1", "image": "linode/debian12",
               "authorized_keys": ["ssh-ed25519 AAAA"]})
    );
    assert_eq!(
        p.resources[1].inputs["inbound"][0],
        json!({"label": "ssh", "action": "ACCEPT", "protocol": "TCP", "ports": "22",
               "addresses": {"ipv4": ["0.0.0.0/0"], "ipv6": ["::/0"]}})
    );
    assert_eq!(
        p.resources[1].inputs["linodes"],
        json!([{"$ref": "linode.instance:web", "$path": "id"}])
    );
    assert_eq!(
        p.resources[3].inputs["domain_id"],
        json!({"$ref": "linode.domain:zone", "$path": "id"})
    );
    assert_eq!(
        p.resources[3].inputs["target"],
        json!({"$ref": "linode.instance:web", "$path": "ipv4"})
    );
}
