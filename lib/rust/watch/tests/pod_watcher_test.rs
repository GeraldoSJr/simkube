use std::collections::HashMap;
use std::sync::{
    Arc,
    Mutex,
};

use cached::{
    Cached,
    SizedCache,
};
use futures::{
    stream,
    StreamExt,
};
use kube::ResourceExt;
use mockall::predicate;
use serde_json::json;
use tracing_test::*;

use super::*;
use crate::k8s::{
    KubeResourceExt,
    PodLifecycleData,
};
use crate::store::MockTraceStorable;
use crate::testutils::fake::{
    apps_v1_discovery,
    make_fake_apiserver,
};
use crate::testutils::*;

const START_TS: i64 = 1234;
const END_TS: i64 = 5678;

#[fixture]
fn clock() -> Box<MockUtcClock> {
    MockUtcClock::new(START_TS)
}

fn make_pod_watcher(
    ns_name: &str,
    clock: Box<MockUtcClock>,
    stored_data: Option<&PodLifecycleData>,
    expected_data: Option<&PodLifecycleData>,
) -> PodWatcher {
    let mut store = MockTraceStorable::new();
    if let Some(data) = expected_data {
        let _ = store
            .expect_record_pod_lifecycle()
            .with(predicate::eq(ns_name.to_string()), predicate::eq(vec![]), predicate::eq(data.clone()))
            .return_const(())
            .once();
    }

    let stored_pods = if let Some(sd) = stored_data {
        HashMap::from([(ns_name.into(), sd.clone())])
    } else {
        HashMap::new()
    };

    let (_, apiset) = make_fake_apiserver();
    PodWatcher::new_from_parts(
        apiset,
        stream::empty().boxed(),
        stored_pods,
        SizedCache::with_size(CACHE_SIZE),
        Arc::new(Mutex::new(store)),
        clock,
    )
}

#[rstest]
#[traced_test]
#[tokio::test]
async fn test_handle_pod_event_applied_empty(test_pod: corev1::Pod, clock: Box<MockUtcClock>) {
    let ns_name = test_pod.namespaced_name();
    let mut pw = make_pod_watcher(&ns_name, clock, None, None);

    let mut evt = Event::Applied(test_pod.clone());

    pw.handle_pod_event(&mut evt).await.unwrap();

    assert_eq!(pw.get_owned_pod_lifecycle(&ns_name), None);
}

#[rstest]
#[traced_test]
#[tokio::test]
async fn test_handle_pod_event_applied(mut test_pod: corev1::Pod, clock: Box<MockUtcClock>) {
    let ns_name = test_pod.namespaced_name();
    let expected_data = PodLifecycleData::Running(START_TS);
    let mut pw = make_pod_watcher(&ns_name, clock, None, Some(&expected_data));

    pods::add_running_container(&mut test_pod, START_TS);
    let mut evt = Event::Applied(test_pod);

    pw.handle_pod_event(&mut evt).await.unwrap();

    assert_eq!(pw.get_owned_pod_lifecycle(&ns_name).unwrap(), expected_data);
}

#[rstest]
#[case::same_ts(START_TS)]
#[case::diff_ts(5555)]
#[traced_test]
#[tokio::test]
async fn test_handle_pod_event_applied_already_stored(
    mut test_pod: corev1::Pod,
    clock: Box<MockUtcClock>,
    #[case] stored_ts: i64,
) {
    let ns_name = test_pod.namespaced_name();
    let stored_data = PodLifecycleData::Running(stored_ts);
    let mut pw = make_pod_watcher(&ns_name, clock, Some(&stored_data), None);

    pods::add_running_container(&mut test_pod, START_TS);
    let mut evt = Event::Applied(test_pod);

    pw.handle_pod_event(&mut evt).await.unwrap();

    assert_eq!(pw.get_owned_pod_lifecycle(&ns_name).unwrap(), stored_data);
}

#[rstest]
#[traced_test]
#[tokio::test]
async fn test_handle_pod_event_applied_running_to_finished(mut test_pod: corev1::Pod, clock: Box<MockUtcClock>) {
    let ns_name = test_pod.namespaced_name();
    let stored_data = PodLifecycleData::Running(START_TS);
    let expected_data = PodLifecycleData::Finished(START_TS, END_TS);
    let mut pw = make_pod_watcher(&ns_name, clock, Some(&stored_data), Some(&expected_data));

    pods::add_finished_container(&mut test_pod, START_TS, END_TS);
    let mut evt = Event::Applied(test_pod);

    pw.handle_pod_event(&mut evt).await.unwrap();

    assert_eq!(pw.get_owned_pod_lifecycle(&ns_name).unwrap(), expected_data);
}

#[rstest]
#[traced_test]
#[tokio::test]
async fn test_handle_pod_event_applied_running_to_finished_wrong_start_ts(
    mut test_pod: corev1::Pod,
    clock: Box<MockUtcClock>,
) {
    let ns_name = test_pod.namespaced_name();
    let stored_data = PodLifecycleData::Running(5555);
    let mut pw = make_pod_watcher(&ns_name, clock, Some(&stored_data), None);

    pods::add_finished_container(&mut test_pod, START_TS, END_TS);
    let mut evt = Event::Applied(test_pod);

    pw.handle_pod_event(&mut evt).await.unwrap();

    assert_eq!(pw.get_owned_pod_lifecycle(&ns_name).unwrap(), stored_data);
}

#[rstest]
#[case::no_data(None)]
#[case::mismatched_data(Some(&PodLifecycleData::Finished(1, 2)))]
#[traced_test]
#[tokio::test]
async fn test_handle_pod_event_deleted_no_update(
    mut test_pod: corev1::Pod,
    mut clock: Box<MockUtcClock>,
    #[case] stored_data: Option<&PodLifecycleData>,
) {
    let ns_name = test_pod.namespaced_name();
    clock.set(END_TS);

    let mut pw = make_pod_watcher(&ns_name, clock, stored_data, None);

    pods::add_running_container(&mut test_pod, START_TS);
    let mut evt = Event::Deleted(test_pod);

    pw.handle_pod_event(&mut evt).await.unwrap();

    assert_eq!(pw.get_owned_pod_lifecycle(&ns_name), None);
}

#[rstest]
#[case::old_still_running(false)]
#[case::old_finished(true)]
#[traced_test]
#[tokio::test]
async fn test_handle_pod_event_deleted_finished(
    mut test_pod: corev1::Pod,
    mut clock: Box<MockUtcClock>,
    #[case] old_finished: bool,
) {
    // If the watcher index says the pod is finished, we've already
    // recorded it in the store, so it really shouldn't matter what the clock says
    let ns_name = test_pod.namespaced_name();
    let finished = PodLifecycleData::Finished(START_TS, END_TS);
    let stored_data = if old_finished { finished.clone() } else { PodLifecycleData::Running(START_TS) };
    let expected_data = if old_finished { None } else { Some(&finished) };
    clock.set(10000);

    let mut pw = make_pod_watcher(&ns_name, clock, Some(&stored_data), expected_data);

    pods::add_finished_container(&mut test_pod, START_TS, END_TS);
    let mut evt = Event::Deleted(test_pod);

    pw.handle_pod_event(&mut evt).await.unwrap();

    assert_eq!(pw.get_owned_pod_lifecycle(&ns_name), None);
}

#[rstest]
#[traced_test]
#[tokio::test]
async fn test_handle_pod_event_deleted_running(mut test_pod: corev1::Pod, mut clock: Box<MockUtcClock>) {
    // Here the pod is still "running" when the delete call comes in, so we
    // expect the end_ts in the lifecycle data to match the current time
    let ns_name = test_pod.namespaced_name();
    let stored_data = PodLifecycleData::Running(START_TS);
    let expected_data = PodLifecycleData::Finished(START_TS, END_TS);
    clock.set(END_TS);

    let mut pw = make_pod_watcher(&ns_name, clock, Some(&stored_data), Some(&expected_data));

    pods::add_running_container(&mut test_pod, START_TS);
    let mut evt = Event::Deleted(test_pod.clone());

    pw.handle_pod_event(&mut evt).await.unwrap();

    assert_eq!(pw.get_owned_pod_lifecycle(&ns_name), None);
}

#[rstest]
#[traced_test]
#[tokio::test]
async fn test_handle_pod_event_deleted_no_container_data(test_pod: corev1::Pod, mut clock: Box<MockUtcClock>) {
    // Same as the test case above, except this time the pod object
    // doesn't include any info about its containers, it just has metadata
    let ns_name = test_pod.namespaced_name();
    let stored_data = PodLifecycleData::Running(START_TS);
    let expected_data = PodLifecycleData::Finished(START_TS, END_TS);
    clock.set(END_TS);

    let mut pw = make_pod_watcher(&ns_name, clock, Some(&stored_data), Some(&expected_data));
    let mut evt = Event::Deleted(test_pod);
    pw.handle_pod_event(&mut evt).await.unwrap();

    assert_eq!(pw.get_owned_pod_lifecycle(&ns_name), None);
}

#[rstest]
#[traced_test]
#[tokio::test]
async fn test_handle_pod_event_restarted(mut clock: Box<MockUtcClock>) {
    let pod_names = ["pod1", "pod2", "pod3"].map(|name| format!("{}/{}", TEST_NAMESPACE, name));
    let pod_lifecycles: HashMap<String, PodLifecycleData> = pod_names
        .iter()
        .map(|ns_name| (ns_name.clone(), PodLifecycleData::Running(START_TS)))
        .collect();

    let mut update_pod1 = test_pod("pod1".into());
    pods::add_finished_container(&mut update_pod1, START_TS, END_TS);
    let mut update_pod2 = test_pod("pod2".into());
    pods::add_running_container(&mut update_pod2, START_TS);

    let clock_ts = clock.set(10000);

    let mut store = MockTraceStorable::new();
    let _ = store
        .expect_record_pod_lifecycle()
        .with(
            predicate::eq("test/pod1"),
            predicate::eq(vec![]),
            predicate::eq(PodLifecycleData::Finished(START_TS, END_TS)),
        )
        .return_const(())
        .once();

    let _ = store
        .expect_record_pod_lifecycle()
        .with(predicate::eq("test/pod2".to_string()), predicate::eq(vec![]), predicate::always())
        .never();

    let _ = store
        .expect_record_pod_lifecycle()
        .with(
            predicate::eq("test/pod3".to_string()),
            predicate::eq(vec![]),
            predicate::eq(PodLifecycleData::Finished(START_TS, clock_ts)),
        )
        .return_const(())
        .once();

    let mut cache = SizedCache::with_size(CACHE_SIZE);
    for ns_name in &pod_names {
        cache.cache_set(ns_name.clone(), vec![]);
    }

    let (_, apiset) = make_fake_apiserver();
    let mut pw = PodWatcher::new_from_parts(
        apiset,
        stream::empty().boxed(),
        pod_lifecycles,
        cache,
        Arc::new(Mutex::new(store)),
        clock,
    );

    let mut evt = Event::Restarted(vec![update_pod1, update_pod2]);

    pw.handle_pod_event(&mut evt).await.unwrap();
    assert_eq!(pw.get_owned_pod_lifecycle("test/pod1").unwrap(), PodLifecycleData::Finished(START_TS, END_TS));
    assert_eq!(pw.get_owned_pod_lifecycle("test/pod2").unwrap(), PodLifecycleData::Running(START_TS));
    assert_eq!(pw.get_owned_pod_lifecycle("test/pod3"), None);
}

#[rstest]
#[traced_test]
#[tokio::test]
async fn test_compute_owner_chain_cached(mut test_pod: corev1::Pod) {
    let rsref = metav1::OwnerReference {
        api_version: "apps/v1".into(),
        kind: "replicaset".into(),
        name: "test-rs".into(),
        uid: "asdfasdf".into(),
        ..Default::default()
    };
    let deplref = metav1::OwnerReference {
        api_version: "apps/v1".into(),
        kind: "deployment".into(),
        name: "test-depl".into(),
        uid: "yuioyoiuy".into(),
        ..Default::default()
    };

    test_pod.owner_references_mut().push(rsref.clone());
    let expected_owners = vec![rsref, deplref];

    let mut cache = SizedCache::with_size(CACHE_SIZE);
    cache.cache_set(test_pod.namespaced_name(), expected_owners.clone());

    let (_, mut apiset) = make_fake_apiserver();
    let res = compute_owner_chain(&mut apiset, &test_pod, &mut cache).await.unwrap();
    assert_eq!(res, expected_owners);
}

#[rstest]
#[traced_test]
#[tokio::test]
async fn test_compute_owner_chain(mut test_pod: corev1::Pod) {
    let rsref = metav1::OwnerReference {
        api_version: "apps/v1".into(),
        kind: "ReplicaSet".into(),
        name: "test-rs".into(),
        uid: "asdfasdf".into(),
        ..Default::default()
    };
    let deplref = metav1::OwnerReference {
        api_version: "apps/v1".into(),
        kind: "Deployment".into(),
        name: "test-depl".into(),
        uid: "yuioyoiuy".into(),
        ..Default::default()
    };

    let (mut fake_apiserver, mut apiset) = make_fake_apiserver();
    fake_apiserver.handle(|when, then| {
        when.path("/apis/apps/v1");
        then.json_body(apps_v1_discovery());
    });

    let rs_owner = deplref.clone();
    fake_apiserver.handle(move |when, then| {
        when.path("/apis/apps/v1/replicasets");
        then.json_body(json!({
            "metadata": {},
            "items": [
                {
                    "metadata": {
                        "namespace": "test",
                        "name": "test-rs",
                        "ownerReferences": [rs_owner],
                    }
                },
            ],
        }));
    });

    fake_apiserver.handle(move |when, then| {
        when.path("/apis/apps/v1/deployments");
        then.json_body(json!({
            "metadata": {},
            "items": [
                {
                    "metadata": {
                        "namespace": "test",
                        "name": "test-depl",
                    }
                },
            ],
        }));
    });
    fake_apiserver.build();

    test_pod.owner_references_mut().push(rsref.clone());
    let res = compute_owner_chain(&mut apiset, &test_pod, &mut SizedCache::with_size(CACHE_SIZE))
        .await
        .unwrap();

    assert_eq!(res, vec![rsref, deplref]);
}