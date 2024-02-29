use std::fs::File;
use std::io::Write;
use std::sync::{
    Arc,
    Mutex,
};

use chrono::Utc;
use simkube::k8s::ApiSet;
use simkube::prelude::*;
use simkube::store::TraceStore;
use simkube::watch::{
    DynObjWatcher,
    PodWatcher,
};

use crate::args;

pub async fn cmd(args: &args::Snapshot) -> EmptyResult {
    println!("Reading config from {}...", args.config_file);
    let config = TracerConfig::load(&args.config_file)?;

    println!("Connecting to kubernetes cluster...");
    let client = kube::Client::try_default().await?;
    let mut apiset = ApiSet::new(client.clone());

    println!("Loading snapshot into store...");
    let store = Arc::new(Mutex::new(TraceStore::new(config.clone())));
    let (dyn_obj_watcher, do_ready_rx) =
        DynObjWatcher::new(store.clone(), &mut apiset, &config.tracked_objects).await?;
    let (pod_watcher, pod_ready_rx) = PodWatcher::new(client, store.clone(), apiset);

    let do_handle = tokio::spawn(dyn_obj_watcher.start());
    let pod_handle = tokio::spawn(pod_watcher.start());

    // the receivers block until they get a message, so don't actually care about the value
    let _ = do_ready_rx.recv();
    let _ = pod_ready_rx.recv();

    do_handle.abort();
    pod_handle.abort();

    // When I don't await the tasks, it seems like it hangs.  I'm not 100% this was actually
    // the issue though, it seemed a bit erratic.
    let _ = do_handle.await;
    let _ = pod_handle.await;

    println!("Exporting snapshot data from store...");
    let filters = ExportFilters::new(args.excluded_namespaces.clone(), vec![], true);
    let start_ts = Utc::now().timestamp();
    let end_ts = start_ts + 1;
    let data = store.lock().unwrap().export(start_ts, end_ts, &filters)?;

    println!("Writing trace file: {}", args.output);
    let mut file = File::create(&args.output)?;
    file.write_all(&data)?;

    println!("Done!");
    Ok(())
}