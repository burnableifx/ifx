//! Real QEMU smoke test. It is deliberately opt-in because it downloads a cloud image
//! on first use and boots a VM. Run with `IFX_QEMU_TESTS=1 cargo test -p ifx --test qemu`.

#![cfg(feature = "qemu")]

use std::path::PathBuf;

use ifx::engine::Engine;
use ifx::model::{Connection, Program, ResourceDecl, Urn};
use ifx::provider::{Ctx, Handler, Registry};
use ifx::providers::qemu::{ImageHandler, InstanceHandler, NetworkHandler, VolumeHandler};
use ifx::state::{Entry, State};
use ifx::transport::{Cmd, TransportPool};
use serde_json::json;

const IMAGE_URL: &str = "https://cloud.debian.org/images/cloud/trixie/20260712-2537/debian-13-genericcloud-amd64-20260712-2537.qcow2";
const IMAGE_SHA256: &str = "2cab162ddebb1ef083cca8f8261f77c93ae70f98252aabfeb1d8a28c30b191b1";

fn enabled() -> bool {
    if std::env::var("IFX_QEMU_TESTS").as_deref() == Ok("1") {
        true
    } else {
        eprintln!("IFX_QEMU_TESTS is not 1; skipping real VM boot");
        false
    }
}

fn test_dir() -> PathBuf {
    std::env::var_os("IFX_QEMU_TEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("ifx-qemu-tests"))
}

#[tokio::test]
async fn boots_debian_and_reopens_temporary_management() {
    if !enabled() {
        return;
    }
    let dir = test_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let transports = TransportPool::default();

    let image_outputs = if let Some(path) = std::env::var_os("IFX_QEMU_TEST_IMAGE") {
        json!({"path": PathBuf::from(path).canonicalize().unwrap()})
    } else {
        let image_urn = Urn::new("qemu.image", "debian-test");
        let image_cx = Ctx {
            urn: &image_urn,
            transports: &transports,
            triggered: false,
        };
        let image_inputs = json!({
            "url": IMAGE_URL,
            "sha256": IMAGE_SHA256,
            "dir": dir,
        });
        let image = ImageHandler;
        match image.read(&image_cx, None, &image_inputs).await.unwrap() {
            Some(actual) if actual.props["sha256"] == IMAGE_SHA256 => actual.outputs,
            Some(actual) => {
                image
                    .update(&image_cx, actual.id.as_deref(), &image_inputs, &actual)
                    .await
                    .unwrap()
                    .outputs
            }
            None => {
                image
                    .create(&image_cx, &image_inputs)
                    .await
                    .unwrap()
                    .outputs
            }
        }
    };

    let network_urn = Urn::new("qemu.network", "boot-test");
    let network_cx = Ctx {
        urn: &network_urn,
        transports: &transports,
        triggered: false,
    };
    let network_inputs = json!({"name": "boot-test", "mode": "socket", "dir": dir});
    let network = NetworkHandler;
    let network_outputs = match network
        .read(&network_cx, None, &network_inputs)
        .await
        .unwrap()
    {
        Some(actual) => actual.outputs,
        None => {
            network
                .create(&network_cx, &network_inputs)
                .await
                .unwrap()
                .outputs
        }
    };

    let volume_urn = Urn::new("qemu.volume", "boot-test-data");
    let volume_cx = Ctx {
        urn: &volume_urn,
        transports: &transports,
        triggered: false,
    };
    let volume_inputs = json!({
        "dir": dir,
        "size_gb": 1,
        "format": "qcow2",
        "preallocation": "off",
    });
    let volume = VolumeHandler;
    let volume_outputs = match volume.read(&volume_cx, None, &volume_inputs).await.unwrap() {
        Some(actual) => actual.outputs,
        None => {
            volume
                .create(&volume_cx, &volume_inputs)
                .await
                .unwrap()
                .outputs
        }
    };

    let instance_urn = Urn::new("qemu.instance", "boot-test");
    let instance_cx = Ctx {
        urn: &instance_urn,
        transports: &transports,
        triggered: false,
    };
    let instance_inputs = json!({
        "dir": dir,
        "image": image_outputs["path"],
        "memory_mb": 768,
        "cpus": 1,
        "disk_gb": 4,
        "authorized_keys": [],
        "user_data": "#cloud-config\n{}\n",
        "networks": [network_outputs["endpoint"]],
        "volumes": [{
            "path": volume_outputs["path"],
            "serial": "ifx-data",
        }],
        "restartable": true,
        "management": "direct_then_remove",
        "egress": "user_nat",
        "ssh_user": "debian",
        "connect_timeout_secs": 300,
        "state": "running",
    });
    let instance = InstanceHandler;
    if instance
        .read(&instance_cx, None, &instance_inputs)
        .await
        .unwrap()
        .is_some()
    {
        instance
            .delete(&instance_cx, None, &instance_inputs)
            .await
            .unwrap();
    }

    let applied = instance
        .create(&instance_cx, &instance_inputs)
        .await
        .unwrap_or_else(|error| panic!("QEMU boot failed: {error:#}"));
    let connection: Connection =
        serde_json::from_value(applied.outputs["connection"].clone()).unwrap();
    let ssh_port = applied.outputs["ssh_port"].as_u64().unwrap() as u16;
    transports.finish_execution().await.unwrap();
    assert!(std::net::TcpListener::bind(("127.0.0.1", ssh_port)).is_ok());

    let transport = transports.get(&connection).await;
    let first_check = transport.exec(&Cmd::new("uname").arg("-s")).await;
    let first_volume_check = transport
        .exec(&Cmd::new("test").args(["-b", "/dev/disk/by-id/virtio-ifx-data"]))
        .await;
    assert!(
        PathBuf::from(&connection.activation().unwrap().lease).is_file(),
        "temporary management did not persist cleanup intent"
    );
    let recovery_engine = Engine::new(Registry::builtin());
    let recovery_program = Program {
        resources: vec![ResourceDecl::new(
            "qemu.instance",
            "boot-test",
            instance_inputs.clone(),
        )],
    };
    let recovered = recovery_engine
        .recover_program_execution_access(&recovery_program, &State::default())
        .await
        .unwrap();
    assert_eq!(recovered.as_slice(), std::slice::from_ref(&instance_urn));
    transports.finish_execution().await.unwrap();
    assert!(std::net::TcpListener::bind(("127.0.0.1", ssh_port)).is_ok());

    let transport = transports.get(&connection).await;
    transport.exec(&Cmd::new("true")).await.unwrap();
    let mut recovery_state = State::default();
    recovery_state.upsert(
        instance_urn.clone(),
        Entry {
            id: applied.id.clone(),
            inputs: instance_inputs.clone(),
            outputs: applied.outputs.clone(),
            depends_on: Vec::new(),
            protect: false,
        },
    );
    let recovered = recovery_engine
        .recover_execution_access(&recovery_state)
        .await
        .unwrap();
    assert_eq!(recovered.as_slice(), std::slice::from_ref(&instance_urn));
    transports.finish_execution().await.unwrap();
    assert!(std::net::TcpListener::bind(("127.0.0.1", ssh_port)).is_ok());

    let running_actual = instance
        .read(&instance_cx, applied.id.as_deref(), &instance_inputs)
        .await
        .unwrap()
        .unwrap();
    let mut paused_inputs = instance_inputs.clone();
    paused_inputs["state"] = json!("paused");
    let paused = instance
        .update(
            &instance_cx,
            applied.id.as_deref(),
            &paused_inputs,
            &running_actual,
        )
        .await
        .unwrap();
    assert_eq!(paused.outputs["state"], "paused");
    let paused_actual = instance
        .read(&instance_cx, applied.id.as_deref(), &paused_inputs)
        .await
        .unwrap()
        .unwrap();
    let resumed = instance
        .update(
            &instance_cx,
            applied.id.as_deref(),
            &instance_inputs,
            &paused_actual,
        )
        .await
        .unwrap();
    assert_eq!(resumed.outputs["state"], "running");
    let running_actual = instance
        .read(&instance_cx, applied.id.as_deref(), &instance_inputs)
        .await
        .unwrap()
        .unwrap();
    let mut stopped_inputs = instance_inputs.clone();
    stopped_inputs["state"] = json!("stopped");
    let stopped = instance
        .update(
            &instance_cx,
            applied.id.as_deref(),
            &stopped_inputs,
            &running_actual,
        )
        .await
        .unwrap();
    assert!(stopped.outputs["pid"].is_null());

    let stopped_actual = instance
        .read(&instance_cx, applied.id.as_deref(), &stopped_inputs)
        .await
        .unwrap()
        .unwrap();
    let restarted = instance
        .update(
            &instance_cx,
            applied.id.as_deref(),
            &instance_inputs,
            &stopped_actual,
        )
        .await
        .unwrap();
    assert!(restarted.outputs["pid"].as_u64().is_some());
    let transport = transports.get(&connection).await;
    let second_check = transport.exec(&Cmd::new("uname").arg("-s")).await;
    transports.finish_execution().await.unwrap();
    let cleanup = instance.delete(&instance_cx, None, &instance_inputs).await;
    let volume_cleanup = volume.delete(&volume_cx, None, &volume_inputs).await;

    let volume_output = first_volume_check
        .unwrap_or_else(|error| panic!("guest data-volume check failed: {error:#}"));
    assert!(volume_output.success(), "{}", volume_output.stderr_str());
    for check in [first_check, second_check] {
        let output = check.unwrap_or_else(|error| panic!("guest SSH command failed: {error:#}"));
        assert!(output.success(), "{}", output.stderr_str());
        assert_eq!(output.stdout_str().trim(), "Linux");
    }
    assert!(
        applied.outputs["ips"][0]
            .as_str()
            .unwrap()
            .starts_with("10.")
    );
    cleanup.unwrap();
    volume_cleanup.unwrap();
}
